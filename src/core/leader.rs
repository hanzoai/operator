//! Lease-based leader election for kube-rs operators.
//!
//! Uses Kubernetes Lease objects (`coordination.k8s.io/v1`). Only the holder
//! of the lease runs the controllers; all other replicas wait and retry every
//! 15 seconds.
//!
//! Generalized from the byte-identical implementations that previously lived
//! in the per-universe `operator/src/leader.rs` files. Each operator
//! configures a unique `lease_name` and `identity_prefix` via `LeaderConfig`.
//!
//! Two properties make the election safe to run at `replicas > 1`:
//!
//! * **Every lease write is conditional.** Both the renew and the takeover pin
//!   the `resourceVersion` read in the same cycle, so of two contenders that
//!   observe the same expired lease exactly one write lands and the other is
//!   rejected 409. Without the precondition both writes succeed and both
//!   contenders conclude they lead.
//! * **Losing the lease ends `run`.** `run` returns instead of looping, which
//!   completes the `select!` in `main` that hosts every controller and drops
//!   them with it. Reconciliation stops in ONE place rather than at ~30
//!   individually-gated call sites, and the process then exits so the
//!   Deployment restarts it into a clean election. A stopped process cannot
//!   write; a paused one only fails to write where someone remembered to check.

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use kube::api::{Api, Patch, PatchParams, PostParams};
use kube::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{info, warn};

/// Default lease duration. Long enough to survive a pod restart but short
/// enough to fail over within a minute.
const LEASE_DURATION_SECONDS: i32 = 30;
const RENEW_INTERVAL_SECS: u64 = 10;
const RETRY_INTERVAL_SECS: u64 = 15;

/// Render a timestamp for a Lease `renewTime`/`acquireTime` field, using the
/// SAME serializer the typed path uses.
///
/// The apiserver validates a Lease's MicroTime fields against the fixed Go
/// layout `2006-01-02T15:04:05.000000Z07:00` and 422-rejects anything that is
/// not RFC3339 with EXACTLY six fractional digits. jiff's bare `Timestamp`
/// Display trims trailing zeros (`.457070` -> `.45707`; a whole-second instant
/// drops the fraction entirely), which is how the original 422 got in: every
/// renew failed, so the lease looked perpetually expired and leader election
/// thrashed (`leaseTransitions` climbing, reconciliation starved).
///
/// The canonical form is not ours to invent — `k8s_openapi`'s `MicroTime`
/// already serializes as `%.6f`, and the typed create path below goes through
/// it. So we round-trip through that one serializer instead of hand-rolling a
/// second formatter: the JSON-merge patch and the typed write are then
/// byte-identical BY CONSTRUCTION rather than by a coincidence someone has to
/// keep re-verifying. One way to render a MicroTime, defined in one place.
///
/// Note the freshness test in `try_acquire_or_renew` compares *parsed*
/// `jiff::Timestamp` values, never serialized strings — so formatting only has
/// to satisfy the apiserver, never our own equality.
fn micro_time_value(ts: jiff::Timestamp) -> serde_json::Value {
    serde_json::to_value(MicroTime(ts)).expect("MicroTime always serializes to an RFC3339 string")
}

/// Whether this identity holds the lease, as of one acquire-or-renew cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    /// The apiserver accepted a write pinned to the observed `resourceVersion`:
    /// this identity holds the lease as of that write.
    Held,
    /// A different identity holds a live lease, or one wrote the lease between
    /// this cycle's read and its write (409). Either way this identity does not
    /// hold it.
    Foreign,
    /// The apiserver did not answer. Whether this identity still holds the
    /// lease is unconfirmed.
    Unknown,
}

/// Whether a lease last renewed at `renewed` is still inside its
/// `duration`-second validity window at `now`.
///
/// The complement of the expiry test a contender applies before taking over
/// (`elapsed > duration`), so a holder stops trusting its lease at the instant
/// `elapsed == duration` — one tick before any contender is willing to take it.
/// The two tests leave no overlap. Pure.
fn within_lease(renewed: jiff::Timestamp, now: jiff::Timestamp, duration: i64) -> bool {
    now.duration_since(renewed).as_secs() < duration
}

/// Whether the election loop must yield this cycle.
///
/// Yielding returns from `run`, which completes main's `select!` and drops every
/// controller with it. The rule is the transition out of leadership, and only
/// that: a standby that never held the lease keeps waiting (`Foreign` is its
/// steady state), and a holder that merely failed to reach the apiserver keeps
/// its lease until the window it last renewed actually closes — otherwise one
/// slow renew would restart the process on every apiserver blip. Pure, so the
/// whole rule is one exhaustive match over three states.
fn must_yield(standing: Standing, leading: bool, within_lease: bool) -> bool {
    match standing {
        Standing::Held => false,
        Standing::Foreign => leading,
        Standing::Unknown => leading && !within_lease,
    }
}

/// Per-operator configuration for leader election. Each operator picks a
/// unique `lease_name` (e.g. `lux-operator-leader`, `hanzo-operator-leader`)
/// so multiple operators can coexist in the same cluster without contending
/// on a single lease.
#[derive(Clone, Debug)]
pub struct LeaderConfig {
    /// Lease object name. Must be unique per operator.
    pub lease_name: String,
    /// Identity prefix used when `HOSTNAME` is not set
    /// (e.g. `lux-operator-`). The PID is appended.
    pub identity_prefix: String,
}

/// Shared flag indicating whether this instance is the leader.
pub struct LeaderElection {
    is_leader: Arc<AtomicBool>,
    identity: String,
    namespace: String,
    client: Client,
    config: LeaderConfig,
}

impl LeaderElection {
    /// Construct a new leader election handle. The identity defaults to the
    /// `HOSTNAME` env var (the pod name in K8s) and falls back to
    /// `<identity_prefix><pid>` for local runs.
    pub fn new(client: Client, namespace: String, config: LeaderConfig) -> Self {
        let identity = std::env::var("HOSTNAME")
            .unwrap_or_else(|_| format!("{}{}", config.identity_prefix, std::process::id()));

        LeaderElection {
            is_leader: Arc::new(AtomicBool::new(false)),
            identity,
            namespace,
            client,
            config,
        }
    }

    /// Returns true if this instance currently holds the leader lease.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Relaxed)
    }

    /// Returns a clone of the is_leader flag for sharing with other tasks.
    pub fn leader_flag(&self) -> Arc<AtomicBool> {
        self.is_leader.clone()
    }

    /// Run the leader election loop.
    ///
    /// Returns when this identity stops leading — on shutdown (releasing the
    /// lease) or on losing it (`must_yield`). Returning is the whole mechanism
    /// by which reconciliation stops: main runs this alongside every controller
    /// in one `select!`, so this future completing drops the controllers, and
    /// main then exits the process for the Deployment to restart into a clean
    /// election. A standby that has not yet won loops here indefinitely.
    pub async fn run(&self, shutdown: tokio::sync::watch::Receiver<bool>) {
        let leases: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut shutdown = shutdown;
        // The start of the cycle whose write the apiserver last accepted, which
        // opened the window in which this identity can still prove it holds the
        // lease. Taken before the write rather than after, so it is a floor: the
        // window closes no later than it truly does, never later.
        let mut renewed: Option<jiff::Timestamp> = None;

        loop {
            let now = jiff::Timestamp::now();
            let standing = match self.try_acquire_or_renew(&leases).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(identity = %self.identity, lease = %self.config.lease_name, error = %e, "Leader election error");
                    Standing::Unknown
                }
            };
            let leading = self.is_leader.load(Ordering::Relaxed);
            let within =
                renewed.is_some_and(|t| within_lease(t, now, LEASE_DURATION_SECONDS as i64));

            if must_yield(standing, leading, within) {
                warn!(
                    identity = %self.identity,
                    lease = %self.config.lease_name,
                    ?standing,
                    "Lost leader lease; stopping the operator so a single leader reconciles"
                );
                self.is_leader.store(false, Ordering::Relaxed);
                return;
            }

            if standing == Standing::Held {
                renewed = Some(now);
                if !leading {
                    info!(identity = %self.identity, lease = %self.config.lease_name, "Acquired leader lease");
                    self.is_leader.store(true, Ordering::Relaxed);
                }
            }

            // A holder retries on the renew interval so an unconfirmed cycle has
            // room to recover before its window closes; a standby polls slower.
            let wait = if standing == Standing::Held || leading {
                RENEW_INTERVAL_SECS
            } else {
                RETRY_INTERVAL_SECS
            };
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(wait)) => {}
                _ = shutdown.changed() => {
                    self.release(&leases).await;
                    return;
                }
            }
        }
    }

    /// Write a lease patch conditional on `rv`, the `resourceVersion` observed
    /// in this same cycle.
    ///
    /// The apiserver treats a `metadata.resourceVersion` in a merge patch as an
    /// optimistic-concurrency precondition and rejects a stale one with 409 —
    /// the same primitive the Service controller pins its status writes with. A
    /// 409 means another contender wrote the lease between this cycle's read and
    /// its write, so this identity's read is stale and it does not hold the
    /// lease: `Foreign`, never `Held`. That rejection is what makes two
    /// contenders racing the same expired lease resolve to one winner.
    async fn write(
        &self,
        leases: &Api<Lease>,
        rv: &str,
        spec: serde_json::Value,
    ) -> anyhow::Result<Standing> {
        let patch = serde_json::json!({
            "metadata": { "resourceVersion": rv },
            "spec": spec,
        });
        match leases
            .patch(
                &self.config.lease_name,
                &PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await
        {
            Ok(_) => Ok(Standing::Held),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(Standing::Foreign),
            Err(e) => Err(e.into()),
        }
    }

    async fn try_acquire_or_renew(&self, leases: &Api<Lease>) -> anyhow::Result<Standing> {
        // k8s-openapi 0.28 backs meta/v1 MicroTime with jiff::Timestamp, so the
        // lease clock is jiff. k8s MicroTime is MICROSECOND precision; jiff's
        // default nanosecond RFC3339 (9 fractional digits) is rejected by the
        // apiserver's Lease validation ("cannot parse ...Z as Z07:00"), so we
        // truncate to microseconds for both the JSON-merge patch and typed writes.
        let now = jiff::Timestamp::now()
            .round(jiff::Unit::Microsecond)
            .unwrap_or_else(|_| jiff::Timestamp::now());
        let lease_name = self.config.lease_name.as_str();

        match leases.get(lease_name).await {
            Ok(existing) => {
                let spec = existing.spec.as_ref();
                let holder = spec.and_then(|s| s.holder_identity.as_deref());
                let renew_time = spec.and_then(|s| s.renew_time.as_ref()).map(|t| t.0);
                let duration = spec
                    .and_then(|s| s.lease_duration_seconds)
                    .unwrap_or(LEASE_DURATION_SECONDS);
                let transitions = spec.and_then(|s| s.lease_transitions).unwrap_or(0);

                let is_expired = match renew_time {
                    Some(t) => now.duration_since(t).as_secs() > duration as i64,
                    None => true,
                };

                // Every write below is conditional on the resourceVersion read
                // above. A live object always carries one; without it the write
                // could not be made safe, so we decline to claim the lease
                // rather than claim it unconditionally.
                let Some(rv) = existing.metadata.resource_version.as_deref() else {
                    warn!(lease = %lease_name, "lease carries no resourceVersion; not claiming");
                    return Ok(Standing::Unknown);
                };

                if holder == Some(self.identity.as_str()) {
                    // Renewing is as conditional as taking over. This branch
                    // trusts a read that a contender may already have superseded
                    // — it patches only `renewTime`, so an unconditional write
                    // would extend the NEW holder's lease and still report
                    // success, leaving both identities believing they lead.
                    self.write(
                        leases,
                        rv,
                        serde_json::json!({ "renewTime": micro_time_value(now) }),
                    )
                    .await
                } else if is_expired {
                    let standing = self
                        .write(
                            leases,
                            rv,
                            serde_json::json!({
                                "holderIdentity": self.identity,
                                "leaseDurationSeconds": LEASE_DURATION_SECONDS,
                                "acquireTime": micro_time_value(now),
                                "renewTime": micro_time_value(now),
                                "leaseTransitions": transitions + 1,
                            }),
                        )
                        .await?;
                    if standing == Standing::Held {
                        info!(
                            identity = %self.identity,
                            previous = holder.unwrap_or("<none>"),
                            lease = %lease_name,
                            "Took over expired leader lease"
                        );
                    }
                    Ok(standing)
                } else {
                    Ok(Standing::Foreign)
                }
            }
            Err(kube::Error::Api(err)) if err.code == 404 => {
                let lease = Lease {
                    metadata: kube::core::ObjectMeta {
                        name: Some(lease_name.to_string()),
                        namespace: Some(self.namespace.clone()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(self.identity.clone()),
                        lease_duration_seconds: Some(LEASE_DURATION_SECONDS),
                        acquire_time: Some(MicroTime(now)),
                        renew_time: Some(MicroTime(now)),
                        lease_transitions: Some(0),
                        // New optional coordinated-lease fields in k8s 1.33's
                        // LeaseSpec — we don't use coordinated leader election.
                        ..Default::default()
                    }),
                };
                // A create is already conditional: of two contenders racing to
                // create the same lease name the apiserver admits one and
                // rejects the other 409 (AlreadyExists). The loser re-reads and
                // finds a live foreign holder on its next cycle.
                match leases.create(&PostParams::default(), &lease).await {
                    Ok(_) => {
                        info!(identity = %self.identity, lease = %lease_name, "Created leader lease");
                        Ok(Standing::Held)
                    }
                    Err(kube::Error::Api(e)) if e.code == 409 => Ok(Standing::Foreign),
                    Err(e) => Err(e.into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn release(&self, leases: &Api<Lease>) {
        if !self.is_leader.load(Ordering::Relaxed) {
            return;
        }
        info!(identity = %self.identity, lease = %self.config.lease_name, "Releasing leader lease");
        let patch = serde_json::json!({
            "spec": {
                "holderIdentity": null,
            }
        });
        let _ = leases
            .patch(
                &self.config.lease_name,
                &PatchParams::default(),
                &Patch::Merge(patch),
            )
            .await;
        self.is_leader.store(false, Ordering::Relaxed);
    }
}

/// A stand-in apiserver serving the Lease endpoints the election uses, with the
/// `resourceVersion` semantics the safety of the election rests on: every write
/// bumps the version, and a merge patch pinning a stale version is rejected 409.
///
/// The election's correctness is a property of its conversation with an
/// apiserver — two contenders interleaving reads and writes — so the tests drive
/// the real `run`/`try_acquire_or_renew` against this, rather than asserting on
/// the flag they happen to set internally.
#[cfg(test)]
mod fake {
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::Router;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    pub struct Fake {
        pub lease: Mutex<Option<Value>>,
        pub version: AtomicU64,
        /// Writes accepted on the canary object — the stand-in for the cluster
        /// writes a controller performs while it believes it leads.
        pub canary: AtomicUsize,
        /// When set, a read blocks until every party has read, so both
        /// contenders observe the same version before either writes.
        pub tie: Mutex<Option<Arc<tokio::sync::Barrier>>>,
    }

    impl Fake {
        pub fn new() -> Arc<Self> {
            Arc::new(Fake {
                lease: Mutex::new(None),
                version: AtomicU64::new(1),
                canary: AtomicUsize::new(0),
                tie: Mutex::new(None),
            })
        }

        /// Install `lease` directly, as a third party would.
        pub fn put(&self, lease: Value) {
            let v = self.version.fetch_add(1, Ordering::SeqCst) + 1;
            let mut l = lease;
            l["metadata"]["resourceVersion"] = json!(v.to_string());
            *self.lease.lock().unwrap() = Some(l);
        }

        pub fn holder(&self) -> Option<String> {
            self.lease.lock().unwrap().as_ref().and_then(|l| {
                l["spec"]["holderIdentity"]
                    .as_str()
                    .map(|s| s.to_string())
            })
        }
    }

    /// RFC 7386 merge: `null` deletes, objects recurse, anything else replaces.
    fn merge(target: &mut Value, patch: &Value) {
        match patch {
            Value::Object(fields) => {
                if !target.is_object() {
                    *target = json!({});
                }
                let t = target.as_object_mut().expect("object");
                for (k, v) in fields {
                    if v.is_null() {
                        t.remove(k);
                    } else {
                        merge(t.entry(k.clone()).or_insert(Value::Null), v);
                    }
                }
            }
            other => *target = other.clone(),
        }
    }

    fn conflict(msg: &str) -> impl IntoResponse {
        (
            StatusCode::CONFLICT,
            [("content-type", "application/json")],
            json!({
                "kind": "Status", "apiVersion": "v1", "status": "Failure",
                "message": msg, "reason": "Conflict", "code": 409
            })
            .to_string(),
        )
    }

    async fn read(State(f): State<Arc<Fake>>, Path(_): Path<(String, String)>) -> impl IntoResponse {
        let snapshot = f.lease.lock().unwrap().clone();
        // Release both contenders only once each has taken its snapshot, so the
        // versions they pin are equal — the exact interleaving a real partition
        // produces, made deterministic.
        let tie = f.tie.lock().unwrap().clone();
        if let Some(b) = tie {
            b.wait().await;
        }
        match snapshot {
            Some(l) => (
                StatusCode::OK,
                [("content-type", "application/json")],
                l.to_string(),
            ),
            None => (
                StatusCode::NOT_FOUND,
                [("content-type", "application/json")],
                json!({
                    "kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "message": "leases.coordination.k8s.io not found",
                    "reason": "NotFound", "code": 404
                })
                .to_string(),
            ),
        }
    }

    async fn write(
        State(f): State<Arc<Fake>>,
        Path(_): Path<(String, String)>,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let patch: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let mut guard = f.lease.lock().unwrap();
        let Some(current) = guard.as_mut() else {
            return conflict("not found").into_response();
        };
        // The precondition. A patch pinning a version other than the live one
        // lost a race and is rejected, exactly as the apiserver does.
        if let Some(pinned) = patch["metadata"]["resourceVersion"].as_str() {
            let live = current["metadata"]["resourceVersion"].as_str().unwrap_or("");
            if pinned != live {
                return conflict("the object has been modified").into_response();
            }
        }
        merge(current, &patch);
        let v = f.version.fetch_add(1, Ordering::SeqCst) + 1;
        current["metadata"]["resourceVersion"] = json!(v.to_string());
        let out = current.clone();
        drop(guard);
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            out.to_string(),
        )
            .into_response()
    }

    async fn create(
        State(f): State<Arc<Fake>>,
        Path(_): Path<String>,
        body: axum::body::Bytes,
    ) -> axum::response::Response {
        let obj: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let mut guard = f.lease.lock().unwrap();
        if guard.is_some() {
            return conflict("already exists").into_response();
        }
        let v = f.version.fetch_add(1, Ordering::SeqCst) + 1;
        let mut l = obj;
        l["metadata"]["resourceVersion"] = json!(v.to_string());
        *guard = Some(l.clone());
        drop(guard);
        (
            StatusCode::CREATED,
            [("content-type", "application/json")],
            l.to_string(),
        )
            .into_response()
    }

    /// A cluster write by something that believes it leads.
    async fn canary(State(f): State<Arc<Fake>>) -> impl IntoResponse {
        f.canary.fetch_add(1, Ordering::SeqCst);
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            json!({"kind": "ConfigMap", "apiVersion": "v1"}).to_string(),
        )
    }

    /// Serve `f` on an ephemeral port; returns the base URL.
    pub async fn serve(f: Arc<Fake>) -> String {
        let app = Router::new()
            .route(
                "/apis/coordination.k8s.io/v1/namespaces/{ns}/leases/{name}",
                get(read).patch(write),
            )
            .route(
                "/apis/coordination.k8s.io/v1/namespaces/{ns}/leases",
                post(create),
            )
            .route("/canary", post(canary))
            .with_state(f);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        format!("http://{addr}")
    }

    pub fn client(url: &str) -> kube::Client {
        // Same install `main` performs, for the same reason: rustls 0.23 compiles
        // both aws-lc-rs (reqwest) and ring (kube), so the process-level
        // CryptoProvider is ambiguous and building a client panics without one.
        // A lib test never runs `main`, so without this the suite passes or fails
        // on whether some other test happened to install one first.
        static PROVIDER: std::sync::Once = std::sync::Once::new();
        PROVIDER.call_once(|| {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        });
        let cfg = kube::Config::new(url.parse().unwrap());
        kube::Client::try_from(cfg).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_clone_is_cheap() {
        let cfg = LeaderConfig {
            lease_name: "test-leader".into(),
            identity_prefix: "test-".into(),
        };
        let dup = cfg.clone();
        assert_eq!(cfg.lease_name, dup.lease_name);
        assert_eq!(cfg.identity_prefix, dup.identity_prefix);
    }

    #[test]
    fn identity_prefix_used_for_pid_fallback() {
        // We can't easily construct LeaderElection without a Client, but we
        // can confirm the prefix flows through the Config struct correctly.
        let cfg = LeaderConfig {
            lease_name: "operator-leader".into(),
            identity_prefix: "operator-".into(),
        };
        assert_eq!(cfg.identity_prefix, "operator-");
    }

    // ========================================================================
    // The yield rule.
    // ========================================================================

    /// A standby's steady state is `Foreign` — every cycle it loses to the live
    /// holder. If losing a lease you never held ended `run`, no replica could
    /// ever stand by to succeed the leader.
    #[test]
    fn standby_waits_rather_than_yielding() {
        assert!(!must_yield(Standing::Foreign, false, false));
        assert!(!must_yield(Standing::Unknown, false, false));
    }

    /// The transition out of leadership is the whole rule.
    #[test]
    fn holder_yields_when_another_identity_holds_the_lease() {
        assert!(must_yield(Standing::Foreign, true, true));
        assert!(must_yield(Standing::Foreign, true, false));
    }

    /// A holder that cannot reach the apiserver keeps its lease until the window
    /// it last renewed closes, then yields. Yielding on the first failed renew
    /// would restart the process on every apiserver blip.
    #[test]
    fn holder_rides_out_a_blip_then_yields_at_expiry() {
        assert!(!must_yield(Standing::Unknown, true, true));
        assert!(must_yield(Standing::Unknown, true, false));
    }

    #[test]
    fn a_confirmed_holder_never_yields() {
        for within in [true, false] {
            for leading in [true, false] {
                assert!(!must_yield(Standing::Held, leading, within));
            }
        }
    }

    /// A holder stops trusting its lease no later than a contender is willing to
    /// take it: `within_lease` goes false at `elapsed == duration`, while the
    /// takeover test admits only `elapsed > duration`. No instant satisfies
    /// both, so the windows cannot overlap.
    #[test]
    fn the_hold_window_closes_before_the_takeover_window_opens() {
        let t0 = jiff::Timestamp::new(1_000_000, 0).unwrap();
        let duration = LEASE_DURATION_SECONDS as i64;
        for elapsed in 0..=(duration + 5) {
            let now = t0 + jiff::SignedDuration::from_secs(elapsed);
            let holder_trusts = within_lease(t0, now, duration);
            let contender_takes = now.duration_since(t0).as_secs() > duration;
            assert!(
                !(holder_trusts && contender_takes),
                "at +{elapsed}s both the holder trusts its lease and a contender takes it"
            );
        }
        assert!(within_lease(t0, t0 + jiff::SignedDuration::from_secs(29), duration));
        assert!(!within_lease(t0, t0 + jiff::SignedDuration::from_secs(30), duration));
    }

    // ========================================================================
    // Contention, against the stand-in apiserver.
    // ========================================================================

    fn election(client: kube::Client, identity: &str) -> LeaderElection {
        let mut e = LeaderElection::new(
            client,
            "operators".to_string(),
            LeaderConfig {
                lease_name: "operator-leader".to_string(),
                identity_prefix: "operator-".to_string(),
            },
        );
        e.identity = identity.to_string();
        e
    }

    fn expired_lease(holder: &str) -> serde_json::Value {
        let old = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(600);
        serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1", "kind": "Lease",
            "metadata": { "name": "operator-leader", "namespace": "operators" },
            "spec": {
                "holderIdentity": holder,
                "leaseDurationSeconds": LEASE_DURATION_SECONDS,
                "acquireTime": micro_time_value(old),
                "renewTime": micro_time_value(old),
                "leaseTransitions": 7,
            }
        })
    }

    /// THE split-brain property. Two contenders observe the same expired lease
    /// at the same version and both move to take it. Exactly one may end up
    /// holding it.
    ///
    /// Without a `resourceVersion` precondition both merge patches are accepted
    /// and both contenders report `Held` — two leaders, each writing to the
    /// cluster. The barrier makes the interleaving deterministic rather than
    /// hoping the race lands.
    #[tokio::test]
    async fn two_contenders_racing_one_expired_lease_produce_one_leader() {
        let f = fake::Fake::new();
        f.put(expired_lease("a-dead-operator"));
        let url = fake::serve(f.clone()).await;
        *f.tie.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));

        let a = election(fake::client(&url), "operator-a");
        let b = election(fake::client(&url), "operator-b");
        let leases_a: Api<Lease> = Api::namespaced(a.client.clone(), &a.namespace);
        let leases_b: Api<Lease> = Api::namespaced(b.client.clone(), &b.namespace);

        let (ra, rb) = tokio::join!(
            a.try_acquire_or_renew(&leases_a),
            b.try_acquire_or_renew(&leases_b),
        );

        let won: Vec<&str> = [(ra.unwrap(), "operator-a"), (rb.unwrap(), "operator-b")]
            .into_iter()
            .filter(|(s, _)| *s == Standing::Held)
            .map(|(_, id)| id)
            .collect();
        assert_eq!(
            won.len(),
            1,
            "exactly one contender may take an expired lease, got {won:?}"
        );
        // The winner is the one the apiserver recorded — no contender concludes
        // it leads on a write that another superseded.
        assert_eq!(f.holder().as_deref(), Some(won[0]));
    }

    /// A renew is as conditional as a takeover. This contender reads a lease it
    /// still holds, a successor takes it over before the renew lands, and the
    /// renew must not report success: it patches only `renewTime`, so accepting
    /// it would extend the successor's lease while telling the stale holder it
    /// still leads.
    #[tokio::test]
    async fn a_renew_racing_a_takeover_does_not_report_success() {
        let f = fake::Fake::new();
        f.put(expired_lease("operator-a"));
        let url = fake::serve(f.clone()).await;
        *f.tie.lock().unwrap() = Some(Arc::new(tokio::sync::Barrier::new(2)));

        // `a` renews (it is the holder); `b` sees the same expired lease and takes over.
        let a = election(fake::client(&url), "operator-a");
        let b = election(fake::client(&url), "operator-b");
        let leases_a: Api<Lease> = Api::namespaced(a.client.clone(), &a.namespace);
        let leases_b: Api<Lease> = Api::namespaced(b.client.clone(), &b.namespace);

        let (ra, rb) = tokio::join!(
            a.try_acquire_or_renew(&leases_a),
            b.try_acquire_or_renew(&leases_b),
        );

        let held: Vec<&str> = [(ra.unwrap(), "operator-a"), (rb.unwrap(), "operator-b")]
            .into_iter()
            .filter(|(s, _)| *s == Standing::Held)
            .map(|(_, id)| id)
            .collect();
        assert_eq!(held.len(), 1, "renew and takeover both succeeded: {held:?}");
        assert_eq!(f.holder().as_deref(), Some(held[0]));
    }

    /// THE stop-writing property, observed as the cluster sees it.
    ///
    /// A controller writes for as long as it is scheduled. It is scheduled by
    /// the same `select!` that hosts the election, so losing the lease must end
    /// `run` — that completes the `select!`, drops the controller, and the
    /// writes stop. If `run` instead loops waiting to re-acquire, the controller
    /// is never dropped and keeps writing beside the new leader: this test hangs
    /// to its timeout and fails.
    #[tokio::test]
    async fn losing_the_lease_stops_the_writes_it_was_hosting() {
        let f = fake::Fake::new();
        f.put(expired_lease("a-dead-operator"));
        let url = fake::serve(f.clone()).await;

        let a = election(fake::client(&url), "operator-a");
        let flag = a.leader_flag();
        let (_tx, rx) = tokio::sync::watch::channel(false);

        let writes = f.clone();
        let endpoint = format!("{url}/canary");
        let stolen = f.clone();

        let hosted = async move {
            tokio::select! {
                // The election.
                _ = a.run(rx) => "election returned",
                // A controller: writes to the cluster for as long as it is scheduled.
                _ = async move {
                    loop {
                        let _ = reqwest::Client::new().post(&endpoint).send().await;
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                } => "controller returned",
                // A successor takes the lease once `a` is leading and writing.
                _ = async move {
                    while !flag.load(Ordering::Relaxed) || writes.canary.load(Ordering::SeqCst) == 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                    let now = jiff::Timestamp::now();
                    let mut fresh = expired_lease("operator-b");
                    fresh["spec"]["renewTime"] = micro_time_value(now);
                    fresh["spec"]["acquireTime"] = micro_time_value(now);
                    stolen.put(fresh);
                    std::future::pending::<()>().await;
                } => "thief returned",
            }
        };

        let ended = tokio::time::timeout(std::time::Duration::from_secs(60), hosted)
            .await
            .expect("run must return when the lease is lost; it looped instead");
        assert_eq!(ended, "election returned");

        // The controller is dropped with the `select!`. What the cluster sees is
        // that the writes stop. Settle first: dropping the controller mid-request
        // can leave one write already counted, which is a stopped controller, not
        // a running one.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let at_yield = f.canary.load(Ordering::SeqCst);
        assert!(
            at_yield > 0,
            "the controller must have been writing to begin with"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(
            f.canary.load(Ordering::SeqCst),
            at_yield,
            "writes continued after the lease was lost"
        );
        // And the successor's lease is intact — the ex-leader never wrote over it.
        assert_eq!(f.holder().as_deref(), Some("operator-b"));
    }

    /// Unwrap the JSON string a Lease patch would carry.
    fn patched(ts: jiff::Timestamp) -> String {
        micro_time_value(ts).as_str().expect("a JSON string").to_string()
    }

    #[test]
    fn micro_time_value_is_always_six_fractional_digits() {
        // (nanoseconds within the second, expected fractional part). All inputs
        // are microsecond-multiples (production rounds to micros before calling).
        let cases = [
            (457_070_000, "457070"), // jiff Display trims to ".45707" — the exact 422 bug
            (500_000_000, "500000"), // Display trims to ".5"
            (0, "000000"),           // whole second — Display drops the fraction entirely
            (45_707_000, "045707"),  // leading zero kept, no trailing zero
            (1_000, "000001"),       // one microsecond
            (999_999_000, "999999"), // max microseconds
        ];
        for (nanos, want_frac) in cases {
            let ts = jiff::Timestamp::new(0, nanos).unwrap();
            let s = patched(ts);
            let frac = s.strip_suffix('Z').unwrap().split_once('.').unwrap().1;
            assert_eq!(
                frac.len(),
                6,
                "{s}: k8s MicroTime needs exactly 6 fractional digits"
            );
            assert!(
                frac.chars().all(|c| c.is_ascii_digit()),
                "{s}: fraction all digits"
            );
            assert_eq!(frac, want_frac, "input nanos={nanos}");
            assert!(s.ends_with(&format!(".{want_frac}Z")));
        }
    }

    /// The regression the whole fix exists to prevent: the JSON-merge patch
    /// (renew/acquire) and the typed `LeaseSpec` write (create) must produce the
    /// SAME bytes. Previously these were two independent formatters that merely
    /// happened to agree; now the patch path delegates to the typed serializer,
    /// so this holds by construction.
    #[test]
    fn patch_and_typed_writes_are_byte_identical() {
        for nanos in [457_070_000, 500_000_000, 0, 45_707_000, 1_000, 999_999_000] {
            let ts = jiff::Timestamp::new(0, nanos).unwrap();

            let spec = LeaseSpec {
                acquire_time: Some(MicroTime(ts)),
                renew_time: Some(MicroTime(ts)),
                ..Default::default()
            };
            let typed = serde_json::to_value(&spec).unwrap();

            assert_eq!(
                typed["renewTime"],
                micro_time_value(ts),
                "patch renewTime must match the typed create write"
            );
            assert_eq!(
                typed["acquireTime"],
                micro_time_value(ts),
                "patch acquireTime must match the typed create write"
            );
        }
    }

    /// Freshness is decided on parsed `jiff::Timestamp` values, never on the
    /// serialized string — so a formatting change can never make a live lease
    /// look expired.
    #[test]
    fn expiry_compares_parsed_instants_not_strings() {
        let now = jiff::Timestamp::new(1_000, 0).unwrap();
        let fresh = jiff::Timestamp::new(995, 0).unwrap(); // 5s old, duration 30 -> alive
        let stale = jiff::Timestamp::new(900, 0).unwrap(); // 100s old -> expired

        assert!(now.duration_since(fresh).as_secs() <= LEASE_DURATION_SECONDS as i64);
        assert!(now.duration_since(stale).as_secs() > LEASE_DURATION_SECONDS as i64);

        // Same instant, differently-spelled fractions parse equal; string
        // comparison would have called these different.
        let a: MicroTime = serde_json::from_value(serde_json::json!("2026-07-26T00:56:07.500000Z")).unwrap();
        let b: MicroTime = serde_json::from_value(serde_json::json!("2026-07-26T00:56:07.5Z")).unwrap();
        assert_eq!(a.0, b.0);
    }
}
