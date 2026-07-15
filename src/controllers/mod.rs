//! Controllers for each CRD Kind.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource;

pub mod base;
pub mod datastore;
pub mod dns;
pub mod gateway;
pub mod ingress;
pub mod kms_zap;
pub mod mpc;
pub mod network;
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

// v0.3.2: new Kinds
pub mod function;
pub mod observability;
pub mod queue;
pub mod spa;
pub mod static_site;

// v0.3.3: facade Kinds (orphaned in v0.3.0 — controllers added here).
pub mod docdb;
pub mod explorer;
pub mod iam;
pub mod indexer;
pub mod kms;
pub mod kv;
pub mod llm;
pub mod s3;
pub mod sql;

// v0.3.4: union with go/ — LuxRuntime + NodeFleet blockchain Kinds.
pub mod luxruntime;
pub mod nodefleet;

// v0.4.x: apps-lifecycle DRIVE controller. Not a CRD Kind — its reconcile
// source is the platform `apps` table (read over `GET /v1/apps`), and it drives
// `declared_tag` → cluster by patching Deployments. Opt-in + fail-safe; see the
// module docs for the safety-gate model. Implements PR 5 of the platform's
// docs/APPS_LIFECYCLE.md.
pub mod apps;

// v0.6.24: native git → CR reconcile. Not a CRD Kind — its reconcile source is
// GIT (`infra/k8s/operator/crs/*.yaml` on hanzoai/universe main). It server-side
// -applies the in-scope CRs so the operator does the whole chain natively
// (git → CR → workload), replacing the external `gitops-reconcile` CronJob.
// Opt-in (GITOPS_RECONCILE_ENABLED) + fail-safe + NEVER prunes; see the module
// docs for the cadence/scope/never-prune model.
pub mod gitops;

// ManagedDatabase facade — per-tenant isolated Datastore workload.
pub mod managed_database;

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
pub mod agent_deployment;

// Tenant controller. Not a CRD Kind — its reconcile source is the set of
// platform-managed tenant namespaces (`tenant-<org>`, labeled
// `hanzo.ai/managed-by=platform`). For each it ensures the namespace-scoped
// `cloud-api-platform` RoleBinding + `ghcr-pull` image-pull Secret that let an
// onboarded org get one-click `/v1/platform` deploy INTO that tenant — and
// nowhere else. Cloud's deploy path blocks on it (waitForTenantRBAC). Enabled by
// default (TENANT_CONTROLLER); see the module docs.
pub mod tenant;
