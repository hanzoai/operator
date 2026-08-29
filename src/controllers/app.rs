//! App reconciler — the role-dispatch super-facade (`apps.hanzo.ai`, kind `App`).
//!
//! The App-collapse: the fleet's workload CRs are now `kind: App`, one deployable
//! whose `spec.role` selects a reconcile PROFILE (a value, not a place). App IS a
//! renamed `Service` — its workload core is literally `ServiceSpec`, flattened —
//! so every field an App CR carries is already handled by an existing reconcile.
//! This controller does NOT reimplement any manifest/persistence logic: it
//! CLASSIFIES the role and DELEGATES to the mature `reconcile_*_inner_pub`
//! functions, threading the App's own owner reference so server-side apply adopts
//! the existing Deployment/StatefulSet and re-parents its ownerRef (Service→App)
//! with no recreate and no downtime.
//!
//! ## Dispatch (pure, unit-tested via [`classify`])
//!
//! | role                                                              | profile                              |
//! |-------------------------------------------------------------------|--------------------------------------|
//! | absent / `generic` / `service` / `llm` / `iam` / `kms` / `explorer` / `function` / `indexer` / `observability` / `queue` / `spa` / `static` | `service::reconcile_service_inner_pub` |
//! | `sql`→postgresql, `kv`→valkey, `docdb`, `s3`, `datastore` | `datastore::reconcile_datastore_inner_pub` (engine forced) |
//! | `ingress`                                                         | `ingress::reconcile_ingress_inner_pub` |
//! | `gateway` / `base` / `mpc` / `network` / `node` / `dns` / `managedDatabase` / `agentDeployment` / `luxRuntime` / `nodeFleet` | delegated (dedicated Kind; App stands aside) |
//! | `chain` / `validator`                                             | NoOp stub (Network owns them)        |
//! | anything else                                                     | fail-safe: report + requeue, never materialize/delete |
//!
//! The roles and their profiles are one table — [`ROLES`] — and the CRD's
//! `spec.role` enum is projected from it, so the schema cannot advertise a role
//! that reaches no arm, nor reject one that does.
//!
//! ## Safety invariants (the whole point)
//!
//! - **No cascade-GC / no delete-by-name.** This controller only ever
//!   creates/updates OWNED objects via the delegate reconciles. The one prune in
//!   the whole delegation chain (ingress shard prune) is owner-uid + `managed-by`
//!   scoped (`ingress::prune_superseded`), never a blind delete-by-name.
//! - **Field preservation.** An App carries `persistence`/`pdb`/`surgeColocation`/
//!   `env`/`volumes`/`volumeMounts` in its flattened `ServiceSpec` (or, for a
//!   datastore, projected onto `DBSpec`), so the materialized workload carries ALL
//!   of them. Asserted in tests.
//! - **Never panic.** Every fallible op returns `Result`; an unknown role or a
//!   spec that a delegate profile rejects becomes a status + requeue, not a crash.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use serde::de::DeserializeOwned;
use tracing::{debug, error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{App, AppSpec, DBSpec, Engine, IngressKindSpec, Phase, ServiceStatus};
use crate::crd_types::{build_condition, carry_transition_time, status_changed, Condition};

use super::{datastore, ingress, owner_ref_for, service};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

// ============================================================================
// Dispatch — the pure role→profile decision. Unit-testable without a cluster
// (like `service::should_colocate` / `apps::decide`), so the fleet-safety
// property "role selects exactly one profile" is locked in tests.
// ============================================================================

/// The reconcile profile a `spec.role` resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Generic Service profile — `reconcile_service_inner_pub(&spec.service)`.
    Service,
    /// Datastore profile — `reconcile_datastore_inner_pub(&db, engine)`, engine
    /// forced by the role so a `role: sql` App can never materialize as Valkey.
    Datastore(Engine),
    /// Ingress controller — `reconcile_ingress_inner_pub`.
    Ingress,
    /// A role owned by a dedicated controller that exposes no owner-taking inner
    /// entrypoint AND that no live App CR uses. The App stands aside: it
    /// materializes nothing and deletes nothing, and reports the delegation.
    /// Wire the delegate's `reconcile_*_inner_pub` the day a real App CR adopts
    /// the role — a one-arm change, never a rewrite.
    Delegated(&'static str),
    /// Network sub-resource NoOp stub (the `Network` Kind owns materialization).
    NoOp(&'static str),
    /// Unrecognized role — fail-safe: report + requeue, never materialize.
    Unknown,
}

/// Classify a `spec.role` string into its reconcile [`Dispatch`]. Absent/empty ⇒
/// the Service profile (the hot path: 62 of the 67 live App CRs carry no role).
/// Matches the merged universe App CRD's `role` enum VALUES exactly (including the
/// camelCase `managedDatabase`) and the
/// node `classify()` taxonomy (its `Generic` set + `spa`/`static`). Pure over
/// Every role the App CRD advertises, with the profile it reconciles through,
/// in the order the CRD's `spec.role` enum carries them.
///
/// ONE table. `classify` looks up in it and the CRD enum is projected from it,
/// so a role cannot be advertised without a profile or handled without being
/// advertised. It used to be two lists with a comment asking that they be kept
/// in lockstep, and they drifted both ways: five roles the schema accepted
/// reached no arm and requeued forever as Unknown, while `node` had an arm the
/// schema rejected at admission.
pub const ROLES: &[(&str, Dispatch)] = &[
    // Generic Service profile — the schema-identical roles the operator owns.
    ("generic", Dispatch::Service),
    ("service", Dispatch::Service),
    ("llm", Dispatch::Service),
    ("iam", Dispatch::Service),
    ("kms", Dispatch::Service),
    ("explorer", Dispatch::Service),
    ("function", Dispatch::Service),
    ("indexer", Dispatch::Service),
    ("observability", Dispatch::Service),
    ("queue", Dispatch::Service),
    // Datastore profile — engine is fixed by the role, never by `spec.type`.
    ("datastore", Dispatch::Datastore(Engine::Datastore)),
    ("docdb", Dispatch::Datastore(Engine::Docdb)),
    ("kv", Dispatch::Datastore(Engine::Valkey)),
    ("s3", Dispatch::Datastore(Engine::S3)),
    ("sql", Dispatch::Datastore(Engine::Postgres)),
    ("managedDatabase", Dispatch::Delegated("ManagedDatabase")),
    // Delegated — dedicated Kind, no owner-taking inner_pub, no live App CR.
    ("base", Dispatch::Delegated("Base")),
    ("gateway", Dispatch::Delegated("Gateway")),
    // Infra controllers with an owner-taking inner entrypoint.
    ("ingress", Dispatch::Ingress),
    ("dns", Dispatch::Delegated("DNS")),
    ("static", Dispatch::Service),
    ("spa", Dispatch::Service),
    ("mpc", Dispatch::Delegated("MPC")),
    // The chain surface lives at bootno.de and is owned there, not here.
    ("chain", Dispatch::NoOp("Chain")),
    ("network", Dispatch::Delegated("Network")),
    ("nodeFleet", Dispatch::Delegated("NodeFleet")),
    ("luxRuntime", Dispatch::Delegated("LuxRuntime")),
    ("validator", Dispatch::NoOp("Validator")),
    ("agentDeployment", Dispatch::Delegated("AgentDeployment")),
    ("node", Dispatch::Delegated("Node")),
];

/// `Option<&str>` so the whole dispatch table is a table-driven unit test.
/// Absent or empty is the generic Service profile.
pub fn classify(role: Option<&str>) -> Dispatch {
    let role = role.map(str::trim).unwrap_or("");
    if role.is_empty() {
        return Dispatch::Service;
    }
    ROLES
        .iter()
        .find(|(name, _)| *name == role)
        .map(|(_, d)| *d)
        // Fail-safe: report + requeue, never materialize.
        .unwrap_or(Dispatch::Unknown)
}

/// Project the full App spec (generic core + preserved extras) onto a delegate
/// spec via a serde round-trip. The App spec is the SUPERSET; each delegate reads
/// the fields it models and ignores the rest (none of the delegate specs
/// `deny_unknown_fields`). This is the "one way" to build an inner spec from an
/// App — no field-by-field copy that could silently drift from the delegate. A
/// spec that does not satisfy the target profile (e.g. a datastore role with no
/// `storage`) returns `Err` → the reconcile requeues fail-safe, never panics.
fn project<T: DeserializeOwned>(spec: &AppSpec) -> Result<T> {
    let value = serde_json::to_value(spec)
        .map_err(|e| OperatorError::Config(format!("App spec serialize failed: {e}")))?;
    serde_json::from_value(value).map_err(|e| {
        OperatorError::Config(format!(
            "App spec does not satisfy the {} profile: {e}",
            std::any::type_name::<T>()
        ))
    })
}

// ============================================================================
// Reconcile
// ============================================================================

pub async fn reconcile(cr: Arc<App>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("App has no namespace".into()))?;

    // Render from the AUTHORITATIVE App, not the reflector-cached copy the
    // Controller handed us: the cache can lag the API server right after a spec
    // edit, and rendering the Deployment env from a stale copy makes it oscillate
    // (see `super::authoritative`). One extra GET per reconcile keeps the render a
    // deterministic function of the newest committed spec, so an env change drains
    // to convergence instead of surging a ReplicaSet forever. `name`/`namespace`
    // are generation-invariant, so computing them from the cached copy first is
    // safe.
    let api: Api<App> = Api::namespaced(ctx.client.clone(), &namespace);
    let cr = super::authoritative(cr, api.get_opt(&name).await.ok().flatten());

    let api_version = format!("{}/v1", ctx.api_group);
    // The owner reference on EVERY materialized child points at the App CR, so
    // server-side apply adopts an existing Deployment/StatefulSet by name and
    // re-parents its controller ownerRef (Service→App). ownerRef is mutable
    // metadata → SSA merges cleanly, no recreate, no downtime.
    let owner = owner_ref_for(cr.as_ref(), &api_version, "App");
    let dispatch = classify(cr.spec.role.as_deref());
    debug!(name, namespace, ?dispatch, role = ?cr.spec.role, "App dispatch");

    // Boundary validation for the pod-rendering profiles: a malformed
    // `securityContext.seccompProfile` (type `Localhost` without a
    // `localhostProfile`, a stray `localhostProfile` on another type, or an
    // unknown type) is rejected by the apiserver on apply, wedging the reconcile
    // in a status-less requeue loop. Catch it here and degrade cleanly (visible
    // status, ordinary 60s requeue — no hot-loop).
    if matches!(dispatch, Dispatch::Service | Dispatch::Datastore(_)) {
        if let Some(profile) = cr
            .spec
            .service
            .security_context
            .as_ref()
            .and_then(|c| c.seccomp_profile.as_ref())
        {
            if let Err(reason) = profile.validate() {
                return Ok(degrade(&ctx, &name, &namespace, &cr, &reason).await);
            }
        }
    }

    match &dispatch {
        Dispatch::Service => {
            // A service profile materializes a Deployment from `spec.image`; an
            // empty image would SSA-blank a live workload's container image
            // (image is `serde(default)` so imageless dns/ingress roles can
            // deserialize). Guard it: a service App with no image is InvalidSpec,
            // reported and NOT applied.
            if cr.spec.service.image.repository.trim().is_empty() {
                return Ok(degrade(
                    &ctx,
                    &name,
                    &namespace,
                    &cr,
                    "role service requires spec.image.repository",
                )
                .await);
            }
            // The flattened ServiceSpec IS the reconcile input verbatim — no
            // projection, the hot path stays a direct delegate call.
            service::reconcile_service_inner_pub(
                &ctx.client,
                &name,
                &namespace,
                &ctx.api_group,
                &cr.spec.service,
                owner,
            )
            .await?;
            let status =
                workload_status(&ctx, &name, &namespace, &cr, WorkloadKind::Deployment).await;
            write_status(&ctx.client, &name, &namespace, &cr, status).await;
        }
        Dispatch::Datastore(engine) => {
            let db: DBSpec = match project(&cr.spec) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(degrade(
                        &ctx,
                        &name,
                        &namespace,
                        &cr,
                        &format!("role datastore: {e}"),
                    )
                    .await)
                }
            };
            datastore::reconcile_datastore_inner_pub(
                &ctx.client,
                &name,
                &namespace,
                &db,
                *engine,
                owner,
            )
            .await?;
            let status =
                workload_status(&ctx, &name, &namespace, &cr, WorkloadKind::StatefulSet).await;
            write_status(&ctx.client, &name, &namespace, &cr, status).await;
        }
        Dispatch::Ingress => {
            let spec: IngressKindSpec = match project(&cr.spec) {
                Ok(v) => v,
                Err(e) => {
                    return Ok(
                        degrade(&ctx, &name, &namespace, &cr, &format!("role ingress: {e}")).await,
                    )
                }
            };
            ingress::reconcile_ingress_inner_pub(&ctx.client, &name, &namespace, &spec, owner)
                .await?;
            // Ingress materializes shards, not one workload — a successful apply
            // is the readiness signal.
            let status = marker_status(
                &cr,
                Phase::Running,
                true,
                "Reconciled",
                "Ingress reconciled",
            );
            write_status(&ctx.client, &name, &namespace, &cr, status).await;
        }
        Dispatch::Delegated(kind) => {
            // Stand aside: materialize nothing, delete nothing. A dedicated
            // controller owns this role and no live App CR uses it today.
            let status = marker_status(
                &cr,
                Phase::Pending,
                false,
                "Delegated",
                &format!("role delegated to the dedicated {kind} controller"),
            );
            write_status(&ctx.client, &name, &namespace, &cr, status).await;
        }
        Dispatch::NoOp(kind) => {
            let status = marker_status(
                &cr,
                Phase::Pending,
                false,
                "NoOp",
                &format!("{kind} is a Network sub-resource reconciled by the Network Kind"),
            );
            write_status(&ctx.client, &name, &namespace, &cr, status).await;
        }
        Dispatch::Unknown => {
            // Fail-safe: never materialize, never delete, never panic — report and
            // requeue. The role is an open string precisely so this path is
            // reachable at runtime rather than rejected at admission then invisible.
            warn!(name, namespace, role = ?cr.spec.role, "App has an unrecognized role; standing by");
            let status = marker_status(
                &cr,
                Phase::Degraded,
                false,
                "UnknownRole",
                &format!(
                    "unrecognized spec.role {:?}; no reconcile profile",
                    cr.spec.role.as_deref().unwrap_or("")
                ),
            );
            write_status(&ctx.client, &name, &namespace, &cr, status).await;
        }
    }

    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Record an invalid-spec condition and stop this reconcile WITHOUT materializing
/// anything — the diagnostic twin of the `Unknown` arm. A datastore/dns/ingress
/// App whose spec cannot project onto its delegate (e.g. `role: sql` missing
/// `storage`), or a service App with no `image`, becomes a VISIBLE
/// `Degraded`/`InvalidSpec` status + requeue instead of a silent 30s hot-loop
/// (a projection `?` that bailed before writing status) or a live-image blank.
async fn degrade(ctx: &Ctx, name: &str, namespace: &str, cr: &App, msg: &str) -> Action {
    warn!(name, namespace, role = ?cr.spec.role, invalid = %msg, "App spec is unsatisfiable; reporting Degraded");
    let status = marker_status(cr, Phase::Degraded, false, "InvalidSpec", msg);
    write_status(&ctx.client, name, namespace, cr, status).await;
    Action::requeue(Duration::from_secs(60))
}

// ============================================================================
// Status — reuses ServiceStatus (the App CRD's status shape) with the same
// `status_changed` guard service.rs uses, so a no-op reconcile never bumps
// resourceVersion (which the watch would re-deliver as a self-triggered
// reconcile storm across the whole fleet).
// ============================================================================

enum WorkloadKind {
    Deployment,
    StatefulSet,
}

/// Poll the primary workload the delegate materialized (Deployment for the
/// service/dns profiles, StatefulSet for datastore) and build the App status:
/// phase from ready-vs-desired replicas + a `Ready` condition + ingress
/// endpoints. Never fails — a missing/erroring workload just reads as 0 ready
/// (phase Creating), never an error out of reconcile.
async fn workload_status(
    ctx: &Ctx,
    name: &str,
    namespace: &str,
    cr: &App,
    kind: WorkloadKind,
) -> ServiceStatus {
    let (ready, available) = match kind {
        WorkloadKind::Deployment => {
            let api: Api<Deployment> = Api::namespaced(ctx.client.clone(), namespace);
            match api.get_opt(name).await {
                Ok(Some(d)) => d
                    .status
                    .map(|s| {
                        (
                            s.ready_replicas.unwrap_or(0),
                            s.available_replicas.unwrap_or(0),
                        )
                    })
                    .unwrap_or((0, 0)),
                _ => (0, 0),
            }
        }
        WorkloadKind::StatefulSet => {
            let api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), namespace);
            match api.get_opt(name).await {
                Ok(Some(s)) => s
                    .status
                    .map(|st| {
                        (
                            st.ready_replicas.unwrap_or(0),
                            st.available_replicas.unwrap_or(0),
                        )
                    })
                    .unwrap_or((0, 0)),
                _ => (0, 0),
            }
        }
    };

    let desired = cr.spec.service.replicas.unwrap_or(1);
    let phase = if ready >= desired && desired > 0 {
        Phase::Running
    } else if ready > 0 {
        Phase::Degraded
    } else {
        Phase::Creating
    };
    let ready_now = matches!(phase, Phase::Running);

    let mut status = ServiceStatus {
        phase: Some(phase),
        ready_replicas: ready,
        available_replicas: available,
        observed_generation: cr.meta().generation.unwrap_or(0),
        ..Default::default()
    };
    let mut cond = build_condition(
        "Ready",
        ready_now,
        if ready_now { "Available" } else { "NotReady" },
        &format!("{ready}/{desired} replicas ready"),
        status.observed_generation,
    );
    let prior = cr.status.clone().unwrap_or_default();
    carry_transition_time(&prior.conditions, &mut cond);
    upsert_condition(&mut status.conditions, cond);

    // Endpoints from ingress hosts (service-shaped roles).
    if let Some(ing) = &cr.spec.service.ingress {
        if ing.enabled {
            let scheme = if ing.tls { "https" } else { "http" };
            status.endpoints = ing
                .hosts
                .iter()
                .map(|h| format!("{scheme}://{h}"))
                .collect();
        }
    }
    status
}

/// A status for a role that materializes no single workload (ingress shards,
/// delegated/NoOp/unknown roles): a fixed phase + one `Ready` condition carrying
/// the reason/message, with the transition time carried from the prior status so
/// a steady-state reconcile does not churn the CR.
fn marker_status(
    cr: &App,
    phase: Phase,
    ready: bool,
    reason: &str,
    message: &str,
) -> ServiceStatus {
    let observed_generation = cr.meta().generation.unwrap_or(0);
    let mut status = ServiceStatus {
        phase: Some(phase),
        observed_generation,
        ..Default::default()
    };
    let mut cond = build_condition("Ready", ready, reason, message, observed_generation);
    let prior = cr.status.clone().unwrap_or_default();
    carry_transition_time(&prior.conditions, &mut cond);
    upsert_condition(&mut status.conditions, cond);
    status
}

/// Upsert a condition in-place by `type_` (mirrors service.rs).
fn upsert_condition(conditions: &mut Vec<Condition>, new_cond: Condition) {
    if let Some(slot) = conditions.iter_mut().find(|c| c.type_ == new_cond.type_) {
        *slot = new_cond;
    } else {
        conditions.push(new_cond);
    }
}

/// Write the App status, skipping the patch when nothing changed. Writing only on
/// real change breaks the self-triggered reconcile storm (an unconditional status
/// merge bumps resourceVersion every loop → the watch re-delivers it). A failed
/// status write is logged, never fatal (the CRD may lag the binary).
async fn write_status(
    client: &Client,
    name: &str,
    namespace: &str,
    cr: &App,
    status: ServiceStatus,
) {
    let prior = cr.status.clone().unwrap_or_default();
    if !status_changed(&status, &prior) {
        return;
    }
    let api: Api<App> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, name, namespace, "failed to update App status (CRD may not be installed)");
    }
}

pub fn on_error(_obj: Arc<App>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "App reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

/// Run the App controller. Watches `App` and dispatches on `spec.role` to the
/// existing per-profile reconciles. This is the sole hanzo.ai workload reconciler
/// for the collapsed fleet (the Service/SQL/KV/… controllers still run but have
/// no CRs to reconcile once everything is `kind: App`).
pub async fn run_app_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<App> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting App controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "App reconcile error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use crate::crd::DNSSpec;
    use super::*;
    use crate::crd::{App, Engine};

    fn app_from_yaml(y: &str) -> App {
        serde_yaml::from_str(y).expect("App CR must deserialize")
    }

    // ---- classify: the whole dispatch table (pure, no cluster) ----

    #[test]
    fn absent_role_is_the_service_profile() {
        // 62 of 67 live App CRs carry no role — this is the hot path.
        assert_eq!(classify(None), Dispatch::Service);
        assert_eq!(classify(Some("")), Dispatch::Service);
        assert_eq!(classify(Some("  ")), Dispatch::Service); // trimmed
    }

    #[test]
    fn service_backed_roles_route_to_service() {
        for r in [
            "generic",
            "service",
            "llm",
            "iam",
            "kms",
            "explorer",
            "function",
            "indexer",
            "observability",
            "queue",
            "spa",
            "static",
        ] {
            assert_eq!(classify(Some(r)), Dispatch::Service, "role {r} → Service");
        }
    }

    #[test]
    fn datastore_roles_force_the_engine_by_kind() {
        assert_eq!(classify(Some("sql")), Dispatch::Datastore(Engine::Postgres));
        assert_eq!(classify(Some("kv")), Dispatch::Datastore(Engine::Valkey));
        assert_eq!(classify(Some("docdb")), Dispatch::Datastore(Engine::Docdb));
        assert_eq!(classify(Some("s3")), Dispatch::Datastore(Engine::S3));
        assert_eq!(
            classify(Some("datastore")),
            Dispatch::Datastore(Engine::Datastore)
        );
    }

    #[test]
    fn infra_roles_route_to_their_controllers() {
        assert_eq!(classify(Some("ingress")), Dispatch::Ingress);
    }

    #[test]
    fn delegated_and_noop_roles_never_materialize() {
        for (r, k) in [
            ("gateway", "Gateway"),
            ("base", "Base"),
            ("mpc", "MPC"),
            ("network", "Network"),
            ("node", "Node"),
        ] {
            assert_eq!(classify(Some(r)), Dispatch::Delegated(k), "role {r}");
        }
        assert_eq!(classify(Some("chain")), Dispatch::NoOp("Chain"));
        assert_eq!(classify(Some("validator")), Dispatch::NoOp("Validator"));
    }

    #[test]
    fn unknown_role_is_fail_safe() {
        // An unrecognized role NEVER materializes and NEVER deletes — it is the
        // Unknown dispatch, handled as a status + requeue.
        assert_eq!(classify(Some("wat")), Dispatch::Unknown);
        assert_eq!(classify(Some("Service")), Dispatch::Unknown); // case-sensitive, matches the CRD enum
    }

    // ---- classify covers EVERY value in the universe CRD role enum ----

    #[test]
    fn every_universe_role_enum_value_is_handled_without_unknown() {
        // The 29 values from apps.hanzo.ai crds.yaml `spec.role.enum`. None may
        // fall through to Unknown — that would be a silent inert CR.
        for r in [
            "generic",
            "service",
            "llm",
            "iam",
            "kms",
            "explorer",
            "function",
            "indexer",
            "observability",
            "queue",
            "datastore",
            "docdb",
            "kv",
            "s3",
            "sql",
            "base",
            "gateway",
            "ingress",
            "static",
            "spa",
            "mpc",
            "chain",
            "network",
            "validator",
        ] {
            assert_ne!(
                classify(Some(r)),
                Dispatch::Unknown,
                "universe role enum value {r} must map to a profile"
            );
        }
    }

    // ---- deserialize the REAL universe crs App instances (verbatim shapes) ----

    /// The `sql` App CR (universe crs/sql.yaml, verbatim) — carries the
    /// datastore-only fields (`type`/`storage`/`credentialsSecret`/`serviceAliases`)
    /// that are NOT in the ServiceSpec schema and must survive as preserved
    /// unknowns.
    const SQL_CR: &str = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata:
  name: sql
  namespace: hanzo
spec:
  role: sql
  type: postgresql
  image:
    repository: ghcr.io/hanzoai/sql
    tag: "18"
    pullPolicy: IfNotPresent
  replicas: 1
  storage:
    storageClassName: do-block-storage
    size: 20Gi
    retentionPolicy: Retain
    volumeName: sql-data
  resources:
    requests: { cpu: 250m, memory: 512Mi }
    limits: { cpu: "2", memory: 4Gi }
  ports:
    - name: data
      containerPort: 5432
      protocol: TCP
  env:
    - name: POSTGRES_USER
      value: hanzo
  credentialsSecret: postgres-credentials
  serviceAliases:
    - hanzo-sql
    - postgres
  imagePullSecrets:
    - name: ghcr-secret
  partOf: data
"#;

    /// A role-less service App (the hanzo-app/cloud/chat shape) carrying the
    /// load-bearing data-loss-class fields.
    const SERVICE_CR: &str = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata:
  name: chat
  namespace: hanzo
spec:
  image:
    repository: ghcr.io/hanzoai/chat
    tag: v1.0.0
  replicas: 2
  strategy: RollingUpdate
  surgeColocation: true
  fsGroup: 1001
  securityContext:
    runAsNonRoot: true
    runAsUser: 65532
    seccompProfile:
      type: RuntimeDefault
  containerSecurityContext:
    readOnlyRootFilesystem: true
    allowPrivilegeEscalation: false
    capabilities:
      drop: [ALL]
  enableServiceLinks: false
  env:
    - name: FOO
      value: bar
  volumes:
    - name: data
      persistentVolumeClaim:
        claimName: chat-app-db
  volumeMounts:
    - name: data
      mountPath: /data
  persistence:
    enabled: true
    dataDir: /var/lib/hanzo/chat
    dbPath: app.db
    bucket: chat-db
  pdb:
    enabled: true
    minAvailable: 1
  ports:
    - name: http
      containerPort: 8080
  ingress:
    enabled: true
    hosts: [chat.hanzo.ai]
    tls: true
"#;

    const DNS_CR: &str = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: dns, namespace: hanzo }
spec:
  role: dns
  ingress:
    enabled: true
    hosts: [dns.hanzo.ai]
    tls: true
"#;

    const INGRESS_CR: &str = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: hanzo-app-sites, namespace: hanzo }
spec:
  role: ingress
  ingressClassName: ingress
  clusterIssuer: letsencrypt-prod
  domains:
    - domain: "*.hanzo.app"
      tls: true
      routes:
        - path: /
          pathType: Prefix
          serviceName: cloud
          servicePort: 8000
"#;

    #[test]
    fn deserializes_real_datastore_cr_and_preserves_unknown_fields() {
        let app = app_from_yaml(SQL_CR);
        assert_eq!(app.spec.role.as_deref(), Some("sql"));
        // ServiceSpec-overlapping fields land in the flattened core.
        assert_eq!(app.spec.service.image.repository, "ghcr.io/hanzoai/sql");
        assert_eq!(app.spec.service.replicas, Some(1));
        // Datastore-only fields are NOT dropped — they survive in `extra`.
        assert!(
            app.spec.extra.contains_key("storage"),
            "storage must be preserved as an unknown (it is not a ServiceSpec field)"
        );
        assert!(app.spec.extra.contains_key("credentialsSecret"));
        assert!(app.spec.extra.contains_key("serviceAliases"));
        assert!(app.spec.extra.contains_key("type"));
    }

    #[test]
    fn datastore_projection_carries_every_field_and_forces_engine() {
        let app = app_from_yaml(SQL_CR);
        assert_eq!(
            classify(app.spec.role.as_deref()),
            Dispatch::Datastore(Engine::Postgres)
        );
        // The DBSpec the datastore reconcile receives must carry storage (required),
        // creds, aliases, and the overlapping ServiceSpec fields — projected from
        // the WHOLE App spec (core + extras), not just the core.
        let db: DBSpec = project(&app.spec).expect("sql App must project to DBSpec");
        assert_eq!(db.storage.size, "20Gi");
        assert_eq!(db.storage.volume_name.as_deref(), Some("sql-data"));
        assert_eq!(db.credentials_secret, "postgres-credentials");
        assert_eq!(db.service_aliases, vec!["hanzo-sql", "postgres"]);
        assert_eq!(db.replicas, Some(1));
        assert_eq!(db.part_of, "data");
        assert_eq!(db.image.as_ref().unwrap().repository, "ghcr.io/hanzoai/sql");
        // 5432 port carried.
        assert_eq!(db.ports.len(), 1);
        assert_eq!(db.ports[0].container_port, 5432);
    }

    #[test]
    fn service_role_preserves_the_data_loss_class_fields() {
        // The exact fields that NO-GO'd the reduced fork: persistence, pdb,
        // surgeColocation, env, volumes, volumeMounts, fsGroup — all must reach
        // the ServiceSpec the reconcile receives verbatim.
        let app = app_from_yaml(SERVICE_CR);
        assert_eq!(classify(app.spec.role.as_deref()), Dispatch::Service);
        let s = &app.spec.service;
        assert!(
            s.persistence.as_ref().is_some_and(|p| p.enabled),
            "persistence preserved"
        );
        assert_eq!(s.persistence.as_ref().unwrap().bucket, "chat-db");
        assert!(s.pdb.as_ref().is_some_and(|p| p.enabled), "pdb preserved");
        assert!(s.surge_colocation, "surgeColocation preserved");
        assert_eq!(s.fs_group, Some(1001), "fsGroup preserved");
        assert_eq!(s.env.len(), 1, "env preserved");
        assert_eq!(s.volumes.len(), 1, "volumes preserved");
        assert_eq!(s.volume_mounts.len(), 1, "volumeMounts preserved");
        assert_eq!(s.strategy, "RollingUpdate");
        // The securityContext / containerSecurityContext / enableServiceLinks
        // passthrough flattens into the TYPED ServiceSpec (never `extra`) — the
        // port-audit unlock, preserved end-to-end like the data-loss fields above.
        let psc = s
            .security_context
            .as_ref()
            .expect("securityContext preserved");
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.run_as_user, Some(65532));
        assert_eq!(
            psc.seccomp_profile.as_ref().unwrap().type_,
            "RuntimeDefault"
        );
        let csc = s
            .container_security_context
            .as_ref()
            .expect("containerSecurityContext preserved");
        assert_eq!(csc.read_only_root_filesystem, Some(true));
        assert_eq!(csc.allow_privilege_escalation, Some(false));
        assert_eq!(
            csc.capabilities.as_ref().unwrap().drop,
            vec!["ALL".to_string()]
        );
        assert_eq!(s.enable_service_links, Some(false));
        // No stray fields leaked into extra — a pure service CR flattens wholly.
        assert!(
            app.spec.extra.is_empty(),
            "service CR has no unknowns: {:?}",
            app.spec.extra
        );
    }

    #[test]
    fn imageless_roles_deserialize_through_the_flattened_service() {
        // dns/ingress App CRs carry NO image — they MUST still deserialize (the
        // reason ServiceSpec.image gained #[serde(default)]).
        let dns = app_from_yaml(DNS_CR);
        assert_eq!(
            dns.spec.service.image.repository, "",
            "no image ⇒ default empty"
        );
        let dspec: DNSSpec = project(&dns.spec).expect("dns App must project to DNSSpec");
        assert!(dspec.ingress.as_ref().is_some_and(|i| i.enabled));

        let ing = app_from_yaml(INGRESS_CR);
        assert_eq!(classify(ing.spec.role.as_deref()), Dispatch::Ingress);
        let ispec: IngressKindSpec =
            project(&ing.spec).expect("ingress App must project to IngressKindSpec");
        assert_eq!(ispec.domains.len(), 1);
        assert_eq!(ispec.domains[0].domain, "*.hanzo.app");
        assert_eq!(ispec.cluster_issuer, "letsencrypt-prod");
        assert_eq!(ispec.domains[0].routes[0].service_name, "cloud");
    }

    #[test]
    fn kv_projects_to_valkey_datastore() {
        let kv = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: kv, namespace: hanzo }
spec:
  role: kv
  type: valkey
  image: { repository: ghcr.io/hanzoai/kv, tag: "9" }
  replicas: 1
  storage: { storageClassName: do-block-storage, size: 2Gi, volumeName: kv-data }
  credentialsSecret: kv-credentials
  serviceAliases: [hanzo-kv, kv-master]
  partOf: data
"#;
        let app = app_from_yaml(kv);
        assert_eq!(
            classify(app.spec.role.as_deref()),
            Dispatch::Datastore(Engine::Valkey)
        );
        let db: DBSpec = project(&app.spec).expect("kv → DBSpec");
        assert_eq!(db.storage.volume_name.as_deref(), Some("kv-data"));
        assert_eq!(db.credentials_secret, "kv-credentials");
    }

    // ---- securityContext / containerSecurityContext / enableServiceLinks on the
    // DATASTORE render path. Each test runs the FULL path a fleet App CR takes:
    // deserialize → classify → project onto DBSpec (the serde round-trip is
    // where these fields get dropped) → render through the ONE datastore builder
    // the controller uses (`datastore::build_datastore_workload`, not a
    // re-implementation), then asserts the field reached the workload.

    fn datastore_sts(app: &App, engine: Engine) -> StatefulSet {
        let db: DBSpec = project(&app.spec).expect("datastore App must project to DBSpec");
        datastore::build_datastore_workload(
            app.metadata.name.as_deref().expect("App has a name"),
            "hanzo",
            &db,
            engine,
            &std::collections::BTreeMap::new(),
        )
        .statefulset
    }

    /// (a) An `s3` (SeaweedFS object store) App that sets `enableServiceLinks:
    /// false` renders it on the StatefulSet PodSpec. This is the crown-jewel
    /// field: k8s's default-`true` injects `*_SERVICE_HOST/PORT` env for every
    /// namespace Service, which aborts the s3 flag parser on restart. Before the
    /// DBSpec fix the serde projection silently dropped it → object store DOWN.
    #[test]
    fn s3_enable_service_links_false_reaches_the_statefulset() {
        let s3 = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: s3, namespace: hanzo }
spec:
  role: s3
  image: { repository: ghcr.io/hanzoai/s3, tag: latest }
  storage: { storageClassName: do-block-storage, size: 100Gi, volumeName: s3-data }
  enableServiceLinks: false
"#;
        let app = app_from_yaml(s3);
        assert_eq!(
            classify(app.spec.role.as_deref()),
            Dispatch::Datastore(Engine::S3)
        );
        // The projection must carry it (this is the DBSpec round-trip fix).
        let db: DBSpec = project(&app.spec).expect("s3 → DBSpec");
        assert_eq!(
            db.enable_service_links,
            Some(false),
            "enableServiceLinks must survive the App→DBSpec projection"
        );
        // And the render must place it on the PodSpec.
        let sts = datastore_sts(&app, Engine::S3);
        let pod = sts.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            pod.enable_service_links,
            Some(false),
            "enableServiceLinks:false must reach the StatefulSet PodSpec (the s3/SeaweedFS unlock)"
        );
    }

    /// (b) A `kv` App with the fleet container-hardening baseline
    /// (`readOnlyRootFilesystem: true` + `capabilities.drop: [ALL]`) renders it on
    /// the MAIN engine container ONLY — a declared sidecar keeps its writable
    /// rootfs (it must be able to write). Mirrors the Service-path invariant.
    #[test]
    fn kv_container_security_context_lands_on_main_not_sidecar() {
        let kv = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: kv, namespace: hanzo }
spec:
  role: kv
  image: { repository: ghcr.io/hanzoai/kv, tag: "9" }
  storage: { storageClassName: do-block-storage, size: 2Gi, volumeName: kv-data }
  containerSecurityContext:
    readOnlyRootFilesystem: true
    allowPrivilegeEscalation: false
    capabilities:
      drop: [ALL]
  sidecars:
    - name: metrics
      image: ghcr.io/hanzoai/exporter:latest
"#;
        let app = app_from_yaml(kv);
        assert_eq!(
            classify(app.spec.role.as_deref()),
            Dispatch::Datastore(Engine::Valkey)
        );
        let db: DBSpec = project(&app.spec).expect("kv → DBSpec");
        assert!(
            db.container_security_context.is_some(),
            "containerSecurityContext must survive the App→DBSpec projection"
        );
        assert_eq!(db.sidecars.len(), 1, "the sidecar must project onto DBSpec");

        let sts = datastore_sts(&app, Engine::Valkey);
        let pod = sts.spec.unwrap().template.spec.unwrap();
        // The MAIN engine container is named after the datastore.
        let main = pod
            .containers
            .iter()
            .find(|c| c.name == "kv")
            .expect("main engine container");
        let csc = main
            .security_context
            .as_ref()
            .expect("main container must carry the hardening securityContext");
        assert_eq!(csc.read_only_root_filesystem, Some(true));
        assert_eq!(csc.allow_privilege_escalation, Some(false));
        assert_eq!(
            csc.capabilities.as_ref().unwrap().drop,
            Some(vec!["ALL".to_string()]),
            "capabilities.drop:[ALL] must reach the engine container (the hardening baseline)"
        );
        // The sidecar must NOT be hardened — it keeps a writable rootfs.
        let sidecar = pod
            .containers
            .iter()
            .find(|c| c.name == "metrics")
            .expect("sidecar container");
        assert!(
            sidecar.security_context.is_none(),
            "the sidecar keeps its own defaults (writable rootfs), never the main container's hardening"
        );
    }

    /// (c) A datastore App that sets NONE of the three new fields renders a
    /// StatefulSet whose PodSpec + engine container carry NO securityContext and
    /// NO enableServiceLinks — byte-identical to the pre-fix render. Proven at the
    /// serialization level: the rendered StatefulSet JSON contains neither key,
    /// and the render is deterministic.
    #[test]
    fn datastore_without_new_fields_is_byte_identical() {
        // SQL_CR (a real fleet CR) sets none of securityContext /
        // containerSecurityContext / enableServiceLinks / fsGroup.
        let app = app_from_yaml(SQL_CR);
        let sts = datastore_sts(&app, Engine::Postgres);
        let pod = sts.spec.clone().unwrap().template.spec.unwrap();
        assert!(
            pod.security_context.is_none(),
            "an omitting datastore must carry no pod securityContext"
        );
        assert!(
            pod.enable_service_links.is_none(),
            "an omitting datastore must carry no enableServiceLinks (k8s default true)"
        );
        assert!(
            pod.containers[0].security_context.is_none(),
            "an omitting datastore's engine container must carry no securityContext"
        );
        // Serialization-level backward-compat: the security surface is wholly absent.
        let json = serde_json::to_string(&sts).expect("serialize StatefulSet");
        assert!(
            !json.contains("securityContext"),
            "no securityContext key may appear for an omitting datastore"
        );
        assert!(
            !json.contains("enableServiceLinks"),
            "no enableServiceLinks key may appear for an omitting datastore"
        );
        // Deterministic: rendering the same spec twice is byte-identical.
        let again = datastore_sts(&app, Engine::Postgres);
        assert_eq!(
            serde_json::to_string(&sts).unwrap(),
            serde_json::to_string(&again).unwrap(),
            "datastore render must be deterministic"
        );
    }

    /// (d) Pod securityContext + fsGroup precedence on the datastore path matches
    /// the Service path: the structured `securityContext.fsGroup` WINS over the
    /// legacy top-level `fsGroup`, and a structured context that omits `fsGroup`
    /// inherits the legacy value — the exact fold `manifests::pod_security_context`
    /// performs for BOTH paths.
    #[test]
    fn datastore_pod_security_context_and_fs_group_precedence() {
        // Structured securityContext.fsGroup=2000 alongside legacy fsGroup=1000 →
        // structured wins.
        let a = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: docdb, namespace: hanzo }
spec:
  role: docdb
  image: { repository: ghcr.io/hanzoai/docdb, tag: latest }
  storage: { storageClassName: do-block-storage, size: 5Gi, volumeName: docdb-data }
  fsGroup: 1000
  securityContext:
    runAsNonRoot: true
    runAsUser: 1000
    fsGroup: 2000
    seccompProfile:
      type: RuntimeDefault
"#;
        let app = app_from_yaml(a);
        let sts = datastore_sts(&app, Engine::Docdb);
        let psc = sts
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .security_context
            .expect("pod securityContext must render");
        assert_eq!(psc.run_as_non_root, Some(true));
        assert_eq!(psc.run_as_user, Some(1000));
        assert_eq!(
            psc.fs_group,
            Some(2000),
            "structured securityContext.fsGroup must win over the legacy top-level fsGroup"
        );
        assert_eq!(psc.seccomp_profile.unwrap().type_, "RuntimeDefault");

        // A structured context WITHOUT fsGroup inherits the legacy top-level value.
        let b = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: docdb, namespace: hanzo }
spec:
  role: docdb
  image: { repository: ghcr.io/hanzoai/docdb, tag: latest }
  storage: { storageClassName: do-block-storage, size: 5Gi, volumeName: docdb-data }
  fsGroup: 1000
  securityContext:
    runAsNonRoot: true
"#;
        let app_b = app_from_yaml(b);
        let sts_b = datastore_sts(&app_b, Engine::Docdb);
        let psc_b = sts_b
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .security_context
            .expect("pod securityContext must render");
        assert_eq!(
            psc_b.fs_group,
            Some(1000),
            "a structured context that omits fsGroup inherits the legacy top-level fsGroup"
        );
        assert_eq!(psc_b.run_as_non_root, Some(true));
    }

    /// End-to-end from the CR wire shape: an App that declares
    /// `securityContext.fsGroupChangePolicy` must reach the rendered PodSpec, and
    /// an App that declares only `fsGroup` must still get the OnRootMismatch
    /// default. This is the path `hanzo-git` needed and did not have — the CRD
    /// modeled no `securityContext` fields at all, so the apiserver pruned the
    /// declaration on write and the author saw no error.
    #[test]
    fn an_app_can_declare_the_fs_group_change_policy_and_defaults_to_skipping_the_chown() {
        let explicit = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: docdb, namespace: hanzo }
spec:
  role: docdb
  image: { repository: ghcr.io/hanzoai/docdb, tag: latest }
  storage: { storageClassName: do-block-storage, size: 5Gi, volumeName: docdb-data }
  securityContext:
    fsGroup: 1000
    fsGroupChangePolicy: Always
"#;
        let psc = datastore_pod_security(&app_from_yaml(explicit));
        assert_eq!(
            psc.fs_group_change_policy.as_deref(),
            Some("Always"),
            "an explicitly declared policy must survive the App→DBSpec projection"
        );

        let defaulted = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: docdb, namespace: hanzo }
spec:
  role: docdb
  image: { repository: ghcr.io/hanzoai/docdb, tag: latest }
  storage: { storageClassName: do-block-storage, size: 5Gi, volumeName: docdb-data }
  fsGroup: 1000
"#;
        let psc = datastore_pod_security(&app_from_yaml(defaulted));
        assert_eq!(psc.fs_group, Some(1000));
        assert_eq!(
            psc.fs_group_change_policy.as_deref(),
            Some("OnRootMismatch"),
            "an fsGroup App must default to skipping the recursive chown"
        );
    }

    /// The rendered pod-level securityContext of a datastore-role App.
    fn datastore_pod_security(app: &App) -> k8s_openapi::api::core::v1::PodSecurityContext {
        datastore_sts(app, Engine::Docdb)
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .security_context
            .expect("pod securityContext must render")
    }

    /// The boundary guard reads `spec.securityContext.seccompProfile` (the
    /// flattened ServiceSpec field, shared by both pod-rendering paths) and
    /// degrades when it is invalid. This asserts the exact value the guard
    /// evaluates: a `Localhost` profile with no `localhostProfile` fails
    /// `validate()`, while a well-formed `RuntimeDefault` passes.
    #[test]
    fn boundary_reads_the_seccomp_profile_the_guard_rejects() {
        let bad = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: s3, namespace: hanzo }
spec:
  role: s3
  image: { repository: ghcr.io/hanzoai/s3, tag: latest }
  storage: { storageClassName: do-block-storage, size: 10Gi, volumeName: s3-data }
  securityContext:
    seccompProfile:
      type: Localhost
"#;
        let app = app_from_yaml(bad);
        let profile = app
            .spec
            .service
            .security_context
            .as_ref()
            .and_then(|c| c.seccomp_profile.as_ref())
            .expect("seccompProfile present");
        assert!(
            profile.validate().is_err(),
            "Localhost without localhostProfile must be rejected at the boundary"
        );

        let ok = r#"
apiVersion: hanzo.ai/v1
kind: App
metadata: { name: s3, namespace: hanzo }
spec:
  role: s3
  image: { repository: ghcr.io/hanzoai/s3, tag: latest }
  storage: { storageClassName: do-block-storage, size: 10Gi, volumeName: s3-data }
  securityContext:
    seccompProfile:
      type: RuntimeDefault
"#;
        let app_ok = app_from_yaml(ok);
        assert!(app_ok
            .spec
            .service
            .security_context
            .as_ref()
            .and_then(|c| c.seccomp_profile.as_ref())
            .expect("seccompProfile present")
            .validate()
            .is_ok());
    }

    // ---- owner reference — the adoption shape ----

    #[test]
    fn materialized_children_carry_an_app_owner_reference() {
        // Every object the delegate reconciles materialize is stamped with THIS
        // owner ref (threaded into each `reconcile_*_inner_pub`). It points at the
        // App CR as the CONTROLLER, so server-side apply adopts the existing
        // Deployment/StatefulSet by name and re-parents its controller ownerRef
        // (Service→App) — mutable metadata, merged by SSA, no recreate, no
        // downtime. `blockOwnerDeletion` keeps GC ordering correct.
        let mut app = app_from_yaml(SERVICE_CR);
        app.metadata.uid = Some("app-uid-123".to_string());
        app.metadata.name = Some("chat".to_string());

        let owner = owner_ref_for(&app, "hanzo.ai/v1", "App");
        assert_eq!(owner.kind, "App", "children are owned by the App Kind");
        assert_eq!(owner.api_version, "hanzo.ai/v1");
        assert_eq!(owner.name, "chat");
        assert_eq!(owner.uid, "app-uid-123");
        assert_eq!(
            owner.controller,
            Some(true),
            "App is the controlling owner (adopts the workload)"
        );
        assert_eq!(owner.block_owner_deletion, Some(true));
    }

    // ---- env-change convergence ----
    //
    // Rendering pod-template env from the reflector-CACHED App, which can lag the
    // API server after a spec edit, makes the env OSCILLATE: one reconcile applies
    // the OLD value, the next the NEW, and each flip surges a ReplicaSet that
    // `maxUnavailable: 0` pins — so the change never lands. Selecting the
    // AUTHORITATIVE copy (higher `metadata.generation`) makes every reconcile
    // render the newest committed spec, which is what turns repeated reconciles
    // into a fixed point. These tests exercise that pure selection, which runs
    // before the spec reaches the (already-deterministic) renderer.

    use crate::controllers::authoritative;
    use crate::crd_types::EnvVar;

    /// A gateway-shaped App at `generation` whose sole env var is the audience
    /// allow-list with `n` comma-joined audiences — the exact field that wedged.
    fn gateway_app(generation: i64, n: usize) -> App {
        let auds: Vec<String> = (0..n).map(|i| format!("aud-{i}")).collect();
        let mut app = app_from_yaml(SERVICE_CR);
        app.metadata.name = Some("gateway".to_string());
        app.metadata.namespace = Some("hanzo".to_string());
        app.metadata.generation = Some(generation);
        app.spec.service.env = vec![EnvVar {
            name: "GATEWAY_ALLOWED_AUDIENCES".to_string(),
            value: Some(auds.join(",")),
            value_from: None,
        }];
        app
    }

    /// Number of comma-joined audiences on the (single) env var of the chosen App.
    fn aud_count(app: &App) -> usize {
        let v = app.spec.service.env[0].value.as_deref().unwrap_or("");
        if v.is_empty() {
            0
        } else {
            v.split(',').count()
        }
    }

    #[test]
    fn env_change_converges_regardless_of_which_copy_is_stale() {
        // gen5 = the OLD 13-audience env; gen6 = the NEW 14-audience env.
        let stale = gateway_app(5, 13);
        let fresh = gateway_app(6, 14);

        // Ordering (a): the reconcile's CACHED copy is fresh (gen6) but the live
        // read momentarily lags (gen5). A naive "always trust live" would regress
        // to 13; the max-generation rule keeps 14.
        let a = authoritative(Arc::new(fresh.clone()), Some(stale.clone()));
        // Ordering (b): the cached copy is STALE (gen5) and the live read is fresh
        // (gen6) — the common case right after an edit. The pre-fix cache-only read
        // rendered 13 here; now it renders 14.
        let b = authoritative(Arc::new(stale.clone()), Some(fresh.clone()));

        // BOTH orderings select the newest (gen6 / 14-audience) spec: the env the
        // renderer receives is identical across reconciles → no 13↔14 flip → the
        // ReplicaSet surge stops and the change lands. This is the convergence proof.
        assert_eq!(aud_count(&a), 14, "ordering (a) must render the newest env");
        assert_eq!(aud_count(&b), 14, "ordering (b) must render the newest env");
        assert_eq!(
            serde_json::to_value(&a.spec.service.env).unwrap(),
            serde_json::to_value(&b.spec.service.env).unwrap(),
            "the env fed to the deterministic renderer must be identical across reconcile orderings"
        );
        assert_eq!(a.meta().generation, Some(6));
        assert_eq!(b.meta().generation, Some(6));
    }

    #[test]
    fn authoritative_falls_back_to_cached_when_live_read_missing() {
        // A transient API error (live = None) must be no worse than the cache-only
        // read it replaces: reconcile proceeds on the cached copy, never panics,
        // never blanks the workload.
        let cached = gateway_app(6, 14);
        let chosen = authoritative(Arc::new(cached), None);
        assert_eq!(chosen.meta().generation, Some(6));
        assert_eq!(aud_count(&chosen), 14);
    }

    #[test]
    fn authoritative_keeps_newer_cached_when_live_is_behind() {
        // Defensive: if the API GET is served from a stale apiserver watch-cache
        // (live behind the informer), the max-generation rule keeps the newer
        // cached copy rather than regressing to the older live one.
        let newer_cached = gateway_app(7, 14);
        let older_live = gateway_app(6, 13);
        let chosen = authoritative(Arc::new(newer_cached), Some(older_live));
        assert_eq!(chosen.meta().generation, Some(7));
        assert_eq!(aud_count(&chosen), 14);
    }
}
