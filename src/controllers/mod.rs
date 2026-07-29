//! Controllers for each CRD Kind.

use std::sync::Arc;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource;

pub mod datastore;
// Image automation: registry→git tag bumps (delivery is the cloud deploy engine).
pub mod ingress;
pub mod kms_zap;
pub mod service;

// Managed-upgrade FSM — the deploy discipline (pre-flight → health-gate →
// auto-rollback) the Service controller composes when a CR opts into
// `spec.upgradePolicy` and the operator's `UPGRADE_FSM_ENABLED` gate is on. Not
// a Kind — a pure decision core + pre-flight resource converge, driven by
// `controllers::service`.
pub mod upgrade;

/// Build an OwnerReference pointing at a CR. The CR must have a UID set.
pub fn owner_ref_for<K>(cr: &K, api_version: &str, kind: &str) -> OwnerReference
where
    K: Resource<DynamicType = ()>,
{
    OwnerReference {
        api_version: api_version.to_string(),
        kind: kind.to_string(),
        name: cr.meta().name.clone().unwrap_or_default(),
        uid: cr.meta().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Pick the AUTHORITATIVE copy of a CR to render from: whichever of the
/// controller's reflector-cached object and a fresh API read carries the higher
/// `metadata.generation`.
///
/// kube-rs hands `reconcile` the object from its reflector store, which can lag
/// the API server right after a spec edit — a watch re-list race, or a reconcile
/// re-triggered (requeue / owned-child event / status write) while the cache is
/// briefly behind. Rendering a Deployment's pod-template env from a stale copy
/// makes it OSCILLATE: one reconcile server-side-applies the OLD env, the next
/// applies the NEW, and each flip surges a ReplicaSet that `maxUnavailable: 0`
/// then pins — so the env change never lands (the gateway audience-rollout wedge,
/// 2026-07). `generation` is monotonic and bumps ONLY on spec changes, so
/// preferring the higher generation renders the newest committed spec on EVERY
/// reconcile regardless of which copy is stale; the SSA then converges to a fixed
/// point and the surge stops. Falls back to the cached copy when the live read is
/// missing (transient API error, or the object is mid-delete) — never worse than
/// the cache-only read it replaces.
pub fn authoritative<K>(cached: Arc<K>, live: Option<K>) -> Arc<K>
where
    K: Resource<DynamicType = ()>,
{
    match live {
        Some(l) if l.meta().generation.unwrap_or(0) >= cached.meta().generation.unwrap_or(0) => {
            Arc::new(l)
        }
        _ => cached,
    }
}

// v0.3.2: new Kinds

// v0.3.3: facade Kinds (orphaned in v0.3.0 — controllers added here).
pub mod kv;
pub mod sql;

// v0.3.4: union with go/ — the Node blockchain Kind.

// ManagedDatabase facade — per-tenant isolated Datastore workload.

// App Kind — the role-dispatch super-facade (`apps.hanzo.ai`, kind `App`). The
// App-collapse: the fleet's workload CRs are `kind: App`, one deployable whose
// `spec.role` selects a reconcile PROFILE. Reconciles by DELEGATING to the
// existing `service`/`datastore`/`dns`/`ingress` `reconcile_*_inner_pub`
// functions with the App's own owner reference — no reimplementation; SSA-by-name
// adopts and re-parents the existing workloads (Service→App) with no downtime.
// (Distinct from `apps` above, which is the platform-`apps`-table image driver.)
pub mod app;

// AgentDeployment — the autonomous-bot lifecycle (cloud Agent + visor-bound
// @hanzo/bot machine). Reconcile ACTIONS reach cloud /v1/agents + visor
// /v1/machines over HTTP; provisioning is opt-in + fail-safe (AGENT_DEPLOY_MODE).

// Tenant controller. Not a CRD Kind — its reconcile source is the set of
// platform-managed tenant namespaces (`tenant-<org>`, labeled
// `hanzo.ai/managed-by=platform`). For each it ensures the namespace-scoped
// `cloud-api-platform` RoleBinding + `ghcr-pull` image-pull Secret that let an
// onboarded org get one-click `/v1/platform` deploy INTO that tenant — and
// nowhere else. Cloud's deploy path blocks on it (waitForTenantRBAC). Enabled by
// default (TENANT_CONTROLLER); see the module docs.
pub mod tenant;
