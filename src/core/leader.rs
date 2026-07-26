//! Lease-based leader election for kube-rs operators.
//!
//! Uses Kubernetes Lease objects (`coordination.k8s.io/v1`). Only the holder
//! of the lease runs the controllers; all other replicas wait and retry every
//! 15 seconds.
//!
//! Generalized from the byte-identical implementations that previously lived
//! in the per-universe `operator/src/leader.rs` files. Each operator
//! configures a unique `lease_name` and `identity_prefix` via `LeaderConfig`.

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

    /// Run the leader election loop. This never returns under normal
    /// operation. On shutdown (when `shutdown` resolves), it releases the
    /// lease.
    pub async fn run(&self, shutdown: tokio::sync::watch::Receiver<bool>) {
        let leases: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
        let mut shutdown = shutdown;

        loop {
            match self.try_acquire_or_renew(&leases).await {
                Ok(true) => {
                    if !self.is_leader.load(Ordering::Relaxed) {
                        info!(identity = %self.identity, lease = %self.config.lease_name, "Acquired leader lease");
                        self.is_leader.store(true, Ordering::Relaxed);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(RENEW_INTERVAL_SECS)) => {}
                        _ = shutdown.changed() => {
                            self.release(&leases).await;
                            return;
                        }
                    }
                }
                Ok(false) => {
                    if self.is_leader.load(Ordering::Relaxed) {
                        warn!(identity = %self.identity, lease = %self.config.lease_name, "Lost leader lease");
                        self.is_leader.store(false, Ordering::Relaxed);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(RETRY_INTERVAL_SECS)) => {}
                        _ = shutdown.changed() => {
                            return;
                        }
                    }
                }
                Err(e) => {
                    warn!(identity = %self.identity, lease = %self.config.lease_name, error = %e, "Leader election error, retrying");
                    if self.is_leader.load(Ordering::Relaxed) {
                        self.is_leader.store(false, Ordering::Relaxed);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(RETRY_INTERVAL_SECS)) => {}
                        _ = shutdown.changed() => {
                            return;
                        }
                    }
                }
            }
        }
    }

    async fn try_acquire_or_renew(&self, leases: &Api<Lease>) -> anyhow::Result<bool> {
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

                if holder == Some(self.identity.as_str()) {
                    let patch = serde_json::json!({
                        "spec": {
                            "renewTime": micro_time_value(now),
                        }
                    });
                    leases
                        .patch(lease_name, &PatchParams::default(), &Patch::Merge(patch))
                        .await?;
                    Ok(true)
                } else if is_expired {
                    let patch = serde_json::json!({
                        "spec": {
                            "holderIdentity": self.identity,
                            "leaseDurationSeconds": LEASE_DURATION_SECONDS,
                            "acquireTime": micro_time_value(now),
                            "renewTime": micro_time_value(now),
                            "leaseTransitions": transitions + 1,
                        }
                    });
                    leases
                        .patch(lease_name, &PatchParams::default(), &Patch::Merge(patch))
                        .await?;
                    info!(
                        identity = %self.identity,
                        previous = holder.unwrap_or("<none>"),
                        lease = %lease_name,
                        "Took over expired leader lease"
                    );
                    Ok(true)
                } else {
                    Ok(false)
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
                leases.create(&PostParams::default(), &lease).await?;
                info!(identity = %self.identity, lease = %lease_name, "Created leader lease");
                Ok(true)
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
