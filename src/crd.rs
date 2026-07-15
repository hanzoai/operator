//! Custom Resource Definitions for the Hanzo operator.
//!
//! All 27 Kinds at `hanzo.ai/v1` (the compile-time default). For other
//! universes (lux.cloud, zoo.cloud, osage.cloud), generate CRD YAMLs with
//! the `generate-crd-yaml` binary, which rewrites the group at install time.
//!
//! ## v0.6.0 schema — compat-free, one way only
//!
//! - Legacy `v1alpha1` aliases `HanzoService`/`HanzoDatastore`/`HanzoDNS`
//!   dropped entirely — no compat Kinds, the v1 Kinds are the one way.
//! - `BaseApp` renamed to the bare `Base` (`bases.hanzo.ai`, kind `Base`).
//!
//! ## Schemars + k8s-openapi
//!
//! k8s-openapi structs (EnvVar, Volume, Condition, ...) don't impl
//! `JsonSchema`. We mirror their wire shape in `crate::crd_types` with our
//! own typed wrappers and convert at the boundary inside controllers.

use std::collections::BTreeMap;

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crd_types::{
    Condition, Container, EnvFromSource, EnvVar, LocalObjectReference, SecretReference, Time,
    Volume, VolumeMount,
};

// ============================================================================
// Common types
// ============================================================================

#[allow(clippy::upper_case_acronyms)]
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
pub enum Phase {
    Pending,
    Creating,
    Running,
    Degraded,
    Deleting,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ImageSpec {
    pub repository: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tag: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pull_policy: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequirements {
    /// Map of resource name (e.g. `cpu`, `memory`) to quantity string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requests: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ProbeSpec {
    /// `httpGet` path. Used only when no `exec`/`tcpSocket` handler is set and
    /// `port > 0`.
    #[serde(default)]
    pub path: String,
    /// `httpGet` port. `0` (the default) means "no HTTP handler" — set an
    /// `exec` or `tcpSocket` handler instead for non-HTTP health checks
    /// (Postgres `pg_isready`, Valkey `redis-cli ping`, a Kafka TCP listener).
    /// Optional so a CR can declare an `exec`/`tcpSocket`-only probe; the old
    /// schema made `port` required, which forced every probe to be HTTP and
    /// silently mangled exec/tcpSocket probes into an invalid `httpGet{port:0}`.
    #[serde(default)]
    pub port: i32,
    /// `exec` handler — probe succeeds when the command exits 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecAction>,
    /// `tcpSocket` handler — probe succeeds when the TCP port accepts a
    /// connection. Mutually exclusive with `httpGet`/`exec` (exec wins, then
    /// tcpSocket, then httpGet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tcp_socket: Option<TcpSocketAction>,
    #[serde(default)]
    pub initial_delay_seconds: i32,
    #[serde(default)]
    pub period_seconds: i32,
}

/// `exec` probe handler (mirror of k8s `core/v1.ExecAction`).
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExecAction {
    #[serde(default)]
    pub command: Vec<String>,
}

/// `tcpSocket` probe handler (mirror of k8s `core/v1.TCPSocketAction`).
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct TcpSocketAction {
    pub port: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServicePort {
    pub name: String,
    pub container_port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_port: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub protocol: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct IngressSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ingress_class_name: String,
    #[serde(default = "default_true")]
    pub tls: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cluster_issuer: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_rules: Vec<PathRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zero_trust_policy: Option<ZeroTrustPolicySpec>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PathRule {
    pub path: String,
    pub path_type: String,
    pub port: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ZeroTrustPolicySpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_emails: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_domains: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_groups: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_duration: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub iam_endpoint: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_from: Vec<NetworkPolicyPeer>,
    #[serde(default)]
    pub allow_ingress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_intra_namespace: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct LabelSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_labels: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyPeer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod_selector: Option<LabelSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_selector: Option<LabelSelector>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct AutoscalingSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_replicas: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_cpu_utilization: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_memory_utilization: Option<i32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PodDisruptionBudgetSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_available: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_unavailable: Option<i32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceMonitorSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metrics_port: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub metrics_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub interval: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct KMSSecretRef {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host_api: String,
    pub project_slug: String,
    pub env_slug: String,
    pub secrets_path: String,
    pub credentials_ref: SecretReference,
    #[serde(default)]
    pub resync_interval: i32,
    pub managed_secret_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub storage_class_name: String,
    /// Quantity string (e.g. `"10Gi"`).
    pub size: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub retention_policy: String,
    /// Name of the `volumeClaimTemplate` (and the auto-injected data mount).
    /// Defaults to `"data"`. This is an IMMUTABLE StatefulSet field, so a
    /// datastore adopting a pre-existing StatefulSet MUST set this to the
    /// existing template's name (e.g. `sql-data` / `kv-data`) — otherwise the
    /// apply is rejected (`updates to statefulset spec ... are forbidden`) and
    /// the workload stops reconciling. New datastores can omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_name: Option<String>,
}

/// Durable SeaweedFS-backed SQLite for ANY Service Kind, via the proven
/// `hanzoai/replicate` init-restore + sidecar-stream pattern (the
/// console-sqlite blueprint, generalized into the operator — the "one way"
/// to give a service a persistent SQLite DB).
///
/// When `enabled`, the Service controller auto-injects (the user hand-writes
/// NONE of this): a shared `app-db` volume (PVC if `storage` is set, else
/// emptyDir) mounted at `data_dir` on the main container; a
/// `<service>-replicate-config` ConfigMap holding `replicate.yml`; a
/// `replicate-restore` initContainer (single-DB mode only — directory
/// restore is best-effort via the sidecar); and a `replicate` sidecar that
/// streams the SQLite WAL to SeaweedFS, age-encrypted client-side.
///
/// This is the Service-Kind analog of `ReplicationSpec` (the ZapDB/ZAP leg);
/// it mirrors that spec's S3/age field shape.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PersistenceSpec {
    #[serde(default)]
    pub enabled: bool,
    /// Mount path shared by the main container, the restore init, and the
    /// sidecar, e.g. `/var/lib/hanzo/console`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data_dir: String,
    /// Single-DB file relative to `data_dir`, e.g. `"app.db"`. Used when
    /// `!dir_mode`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub db_path: String,
    /// Per-org/user/project fan-out: `replicate` watches `data_dir` for many
    /// DBs instead of a single file.
    #[serde(default)]
    pub dir_mode: bool,
    /// Glob used in `dir_mode`. Default `**/*.db`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pattern: String,
    /// SeaweedFS bucket, e.g. `console-db` (or `<org>-db`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bucket: String,
    /// S3 key prefix, e.g. `console/app`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_path: String,
    /// S3 endpoint. MUST keep the `http://` scheme — replicate's S3 client
    /// prepends `https://` to a scheme-less endpoint, which the cleartext
    /// in-cluster `s3` service rejects. Default `http://s3.hanzo.svc:9000`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_endpoint: String,
    /// S3 region. Default `us-east-1`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_region: String,
    /// SeaweedFS/MinIO require path-style addressing (subdomain buckets
    /// don't resolve in-cluster). Default `true`.
    #[serde(default = "default_true")]
    pub force_path_style: bool,
    /// K8s Secret with `access-key` / `secret-key`. Default `s3-credentials`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credentials_secret: String,
    /// K8s Secret with `identity` / `recipients` (age keypair). Default
    /// `<service-name>-replicate-age`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub age_secret: String,
    /// `hanzoai/replicate` image. Default the pinned semver.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// PVC size/class for the `app-db` working volume. `None` → emptyDir.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageSpec>,
}

/// Managed-upgrade policy for a Service — the declarative deploy discipline the
/// operator encodes so no human hand-flips an image, hand-watches a rollout, or
/// hand-writes an auto-rollback loop.
///
/// OPT-IN. When absent (the default), the operator applies `spec.image`
/// directly on every reconcile — the historical behavior, unchanged. When
/// `enabled` AND the operator's cluster-wide `UPGRADE_FSM_ENABLED` gate is on, a
/// change to `spec.image` rolls through a state machine (see
/// `controllers::upgrade`):
///
///   1. **Pre-flight** the candidate BEFORE flipping: boot it against a
///      CSI-snapshot CLONE of the live data (or, for a stateless Service, a
///      boot-only pod) and require it to reach the Service's readiness signal.
///      A candidate that cannot boot over real data (a migration crash) FAILS
///      the upgrade — production is never flipped.
///   2. **Health-gate** the rollout: flip the Deployment image, watch readiness
///      within `rolloutDeadlineSeconds`.
///   3. **Auto-rollback**: if the candidate crashloops / misses readiness within
///      the deadline, revert the Deployment to `status.lastGoodImage`
///      automatically and record the failure. Production is never left on a
///      crashing image.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpgradePolicySpec {
    /// Master per-CR switch. `false`/absent ⇒ the operator applies `spec.image`
    /// directly (historical behavior). `true` (+ the `UPGRADE_FSM_ENABLED`
    /// cluster gate) ⇒ image changes roll through the pre-flight → health →
    /// rollback FSM.
    #[serde(default)]
    pub enabled: bool,
    /// Boot the candidate against a CSI-snapshot CLONE of the live data
    /// (mounted at the persistence `dataDir`) BEFORE flipping production. This
    /// is the operator-side analog of cloud's CI migration-smoke: it catches the
    /// index-before-ADD-COLUMN migration-crash class that only manifests over
    /// real historical schema. Default: `true` when the Service has
    /// `persistence`; a stateless Service pre-flights a boot-only candidate pod
    /// (no clone). Set `false` to skip the pre-flight and rely on the
    /// health-gated rollout + auto-rollback alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight: Option<bool>,
    /// Seconds the health-gated rollout may take to reach Ready before the
    /// operator auto-rolls-back to `lastGoodImage`. Default 300, floored at 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout_deadline_seconds: Option<i64>,
    /// Seconds the pre-flight candidate boot may take to reach Ready before the
    /// pre-flight is judged failed (a migration/boot crash). Default 300,
    /// floored at 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preflight_deadline_seconds: Option<i64>,
    /// Boot-only env overlaid on the pre-flight candidate pod so it runs its
    /// migrate + mount path WITHOUT real side effects (no prod notifications /
    /// billing / outbound). e.g. `CLOUD_ENV=smoke`. The pre-flight pod is never
    /// wired to a Service, is isolated by a deny-all-egress NetworkPolicy, mounts
    /// a CLONE (never the live PVC), and — with this env — makes no outbound
    /// calls. REQUIRED for a stateful FSM-enabled Service (the pre-flight boots
    /// the real image with the real master key over a clone of live data): the
    /// operator refuses to start such an upgrade without it. Set your app's
    /// boot-only marker here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot_env: Vec<EnvVar>,
    /// Seconds the candidate must remain continuously healthy in production
    /// (the soak window) AFTER the rollout completes before the upgrade is judged
    /// Succeeded and `lastGoodImage` advances. Catches a candidate that rolls out
    /// healthy then crash-loops under load — it is rolled back instead of
    /// poisoning last-good. Default 60. `0` opts out (commit on first healthy
    /// observation, the pre-soak behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soak_seconds: Option<i64>,
    /// `VolumeSnapshotClass` for the pre-flight CSI snapshot of the live data
    /// PVC. Empty ⇒ the cluster's default VolumeSnapshotClass. A stateful
    /// pre-flight with no snapshot support FAILS the upgrade CLOSED (never flips
    /// without a pre-flight).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub snapshot_class: String,
}

/// Upgrade FSM phase. Absent `status.upgrade` ⇒ Stable (no upgrade in flight).
/// Mirrors `controllers::upgrade::Phase` on the wire.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, PartialEq, Eq)]
pub enum UpgradePhase {
    /// Booting the candidate against a snapshot-clone of real data (or a
    /// boot-only stateless pod); production still runs the current image.
    Preflighting,
    /// Pre-flight passed; the Deployment is flipped to the candidate and the
    /// operator is health-gating the rollout within the deadline.
    Rolling,
    /// The candidate failed its health gate in production; the operator has
    /// reverted the Deployment to `lastGoodImage` and is waiting for recovery.
    RollingBack,
    /// Terminal: the candidate failed (pre-flight crash or rollout timeout) and
    /// production is safe on `lastGoodImage`. The operator will NOT re-attempt
    /// this exact `targetImage`; a NEW `spec.image` reopens the FSM.
    Failed,
}

/// The in-flight upgrade attempt, persisted on `status.upgrade`. Every field is
/// derived from the CR spec + observed cluster on each reconcile, so the FSM is
/// fully resumable across an operator restart — no in-memory state.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeStatus {
    /// The image this upgrade is driving toward (`spec.image` when it began).
    pub target_image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<UpgradePhase>,
    /// RFC3339 — when this upgrade attempt began.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub started_at: String,
    /// RFC3339 — the deadline for the CURRENT phase. Past it ⇒ the phase fails
    /// to its rollback/terminal state.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub deadline_at: String,
    /// The pre-flight Pod (deterministically named by target) booting the
    /// candidate. Present only in Preflighting.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preflight_pod: String,
    /// The CSI clone PVC the pre-flight mounts. GC'd on pre-flight completion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preflight_clone: String,
    /// The CSI VolumeSnapshot the clone derives from. GC'd on pre-flight
    /// completion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preflight_snapshot: String,
    /// RFC3339 — when the rolling candidate was FIRST observed healthy in
    /// production. The soak window (`upgradePolicy.soakSeconds`) must elapse from
    /// this instant of continuous health before the upgrade is judged Succeeded
    /// and `lastGoodImage` advances. Reset to empty whenever the candidate is
    /// observed unhealthy, so a flapping candidate never commits. Present only in
    /// Rolling, after the first healthy observation.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stable_since: String,
    /// Human-readable last transition reason.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// One completed upgrade attempt, appended to `status.upgradeHistory`.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UpgradeRecord {
    /// The candidate image the attempt targeted.
    pub image: String,
    /// The image production ran before the attempt.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub from_image: String,
    /// `Succeeded` | `RolledBack` | `PreflightFailed` | `Superseded`.
    pub result: String,
    /// RFC3339 — when the attempt concluded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

// ============================================================================
// Service Kind
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Service",
    plural = "services",
    namespaced,
    status = "ServiceStatus",
    shortname = "hsvc",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Image","type":"string","jsonPath":".spec.image.repository"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ServiceSpec {
    /// `#[serde(default)]`: image is optional at the wire level so the `App`
    /// Kind (which flattens `ServiceSpec`) deserializes the imageless role
    /// profiles — a `role: dns` / `role: ingress` App CR carries NO `image`
    /// (its image is fixed by the delegate controller). Rejects no existing
    /// `Service`/`App` CR (every workload profile still sets it) and matches the
    /// merged universe App schema, which models `image` as non-required.
    #[serde(default)]
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<ServicePort>,

    // CRITICAL: env/volumes/volumeMounts MUST be honored. Gateway 503
    // root cause was the legacy Go operator dropping these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<EnvFromSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<VolumeMount>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liveness_probe: Option<ProbeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_probe: Option<ProbeSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoscaling: Option<AutoscalingSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdb: Option<PodDisruptionBudgetSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<NetworkPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_monitor: Option<ServiceMonitorSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kms_secrets: Vec<KMSSecretRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<Container>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<LocalObjectReference>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_account_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: String,
    /// Opt in to a zero-downtime, same-node handoff for a RollingUpdate service
    /// whose data is a single ReadWriteOnce PVC. When true (and strategy is not
    /// Recreate and a PVC is mounted) the operator injects a soft self-podAffinity
    /// (`manifests::colocation_affinity`) so the surge pod co-locates on the
    /// volume's node and bind-mounts the already-attached volume — no Multi-Attach
    /// deadlock, no reattach gap. ONLY set this for a store safe under a brief
    /// same-host two-pod overlap (SQLite WAL + busy_timeout). An exclusive-lock
    /// single-open engine (Badger/LMDB/Meili, Qdrant) must use `strategy: Recreate`
    /// instead — leave this false. Default false (untouched).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub surge_colocation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub part_of: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub component: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init_containers: Vec<Container>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Durable SeaweedFS-backed SQLite via `hanzoai/replicate`. ONE field
    /// auto-wires the restore init + replication sidecar + ConfigMap + PVC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<PersistenceSpec>,
    /// Pod-level `securityContext.fsGroup`. Set this when a NON-root image
    /// (e.g. `esign` runs as uid 1001) must write a `persistence` PVC: the
    /// kubelet chowns the volume to this GID + adds it to every container's
    /// supplementary groups, so the app can write. Omit for root images
    /// (e.g. `console`), which already write any volume. Opt-in so changing
    /// it never restarts unrelated persistence services.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_group: Option<i64>,
    /// Managed-upgrade policy. OPT-IN: when absent, `spec.image` is applied
    /// directly (historical behavior). When `enabled` (+ the operator's
    /// `UPGRADE_FSM_ENABLED` gate), an image change rolls through the
    /// pre-flight → health-gate → auto-rollback FSM (see `controllers::upgrade`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_policy: Option<UpgradePolicySpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServiceStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default)]
    pub available_replicas: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<String>,
    /// The last image proven healthy in production — the auto-rollback target
    /// for the managed-upgrade FSM. Adopted from the running image the first
    /// time the operator observes the Service healthy; advanced to the candidate
    /// only after the candidate is proven healthy in production.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_good_image: Option<String>,
    /// The in-flight upgrade FSM state (absent ⇒ Stable). Fully derived from the
    /// spec + cluster each reconcile, so the FSM resumes across operator restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<UpgradeStatus>,
    /// Bounded history (most-recent-last) of concluded upgrade attempts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upgrade_history: Vec<UpgradeRecord>,
}

// ============================================================================
// Datastore Kind
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackupSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_bucket: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_credentials_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<i32>,
}

/// The datastore engine — the single source of truth for "which datastore".
/// Fixed by Kind for `SQL`/`KV`/`DocDB`/`S3`/`Datastore`; carried as a
/// tenant-chosen field only by `ManagedDatabase` (the one Kind whose engine is
/// not fixed by its Kind). Every per-engine default (image, port, data path,
/// DSN scheme, component label) keys off this value — see
/// `controllers::datastore`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum Engine {
    /// PostgreSQL (`hanzoai/sql`).
    #[serde(rename = "postgresql")]
    #[default]
    Postgres,
    /// Valkey (`hanzoai/kv`).
    Valkey,
    /// FerretDB over Postgres — MongoDB wire protocol (`hanzoai/docdb`).
    Docdb,
    /// MinIO S3 (`hanzoai/s3`).
    Minio,
    /// hanzoai/datastore analytics engine (ClickHouse).
    Datastore,
}

impl Engine {
    /// Canonical identity string — the `app.kubernetes.io/component` label and
    /// the default service-port name. Preserved 1:1 from the retired
    /// `spec.type` discriminator so an adopted StatefulSet's pod spec stays
    /// byte-identical (no needless rollout).
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgresql",
            Engine::Valkey => "valkey",
            Engine::Docdb => "docdb",
            Engine::Minio => "minio",
            Engine::Datastore => "datastore",
        }
    }
}

/// The shared datastore workload VALUE: everything the StatefulSet + Service +
/// PVC machinery needs, independent of engine. The engine is expressed by the
/// Kind (`SQL`/`KV`/`DocDB`/`S3`/`Datastore`) or, for `ManagedDatabase`, by an
/// explicit `engine` field — never braided into this value as a discriminator.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct DBSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    pub storage: StorageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<EnvFromSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<VolumeMount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sidecars: Vec<Container>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credentials_secret: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kms_secrets: Vec<KMSSecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<BackupSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub service_aliases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<ServicePort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<LocalObjectReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<NetworkPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_monitor: Option<ServiceMonitorSpec>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub part_of: String,
    /// Pod-level `securityContext.fsGroup`. Set this when a NON-root engine
    /// image (e.g. FerretDB `docdb` runs as uid:gid 1000, distroless — no
    /// entrypoint can chown) must write its data PVC: the kubelet chowns the
    /// mounted volume to this GID + adds it to every container's supplementary
    /// groups, so the app can write. Omit for root or self-chowning images
    /// (e.g. ClickHouse `datastore`), which already write any volume. Opt-in so
    /// changing it never restarts unrelated datastores (a datastore that leaves
    /// it None gets a byte-identical StatefulSet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_group: Option<i64>,
}

/// `Datastore` Kind — the `hanzoai/datastore` analytics engine (ClickHouse).
/// A concrete engine, not a generic catch-all: the engine IS the Kind.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Datastore",
    plural = "datastores",
    namespaced,
    status = "DatastoreStatus",
    shortname = "hds",
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct DatastoreSpec(pub DBSpec);

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct DatastoreStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub connection_string: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_backup: Option<Time>,
}

// ============================================================================
// Gateway Kind
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRoute {
    pub prefix: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<String>,
    #[serde(default)]
    pub strip_prefix: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimit>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub auth_policy: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RateLimit {
    pub name: String,
    pub max_rate: i32,
    pub every: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_max_rate: Option<i32>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AuthPolicy {
    pub name: String,
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub iam_endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub jwks_url: String,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Gateway",
    plural = "gateways",
    namespaced,
    status = "GatewayStatus",
    shortname = "hgw"
)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<GatewayRoute>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limits: Vec<RateLimit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_policies: Vec<AuthPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_monitor: Option<ServiceMonitorSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub route_count: i32,
}

// ============================================================================
// MPC Kind
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MPCDashboardSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MPCCacheSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "MPC",
    plural = "mpcs",
    namespaced,
    status = "MPCStatus",
    shortname = "hmpc"
)]
#[serde(rename_all = "camelCase")]
pub struct MPCSpec {
    pub image: ImageSpec,
    pub replicas: i32,
    pub threshold: i32,
    #[serde(default)]
    pub p2p_port: i32,
    #[serde(default)]
    pub api_port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dashboard: Option<MPCDashboardSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<MPCCacheSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct MPCStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready_nodes: i32,
    #[serde(default)]
    pub keys_generated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ============================================================================
// Network Kind
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorSpec {
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bootstrap_nodes: Vec<String>,
    #[serde(default)]
    pub staking_port: i32,
    #[serde(default)]
    pub http_port: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChainSpec {
    pub name: String,
    pub vm_id: String,
    pub genesis: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub network_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubServiceSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend_image: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub frontend_image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postgres_storage: Option<StorageSpec>,
}

/// ChainRef is an opaque reference to one blockchain hosted by a Network.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChainRef {
    #[serde(rename = "blockchainID")]
    pub blockchain_id: String,
    #[serde(rename = "vmID", default, skip_serializing_if = "String::is_empty")]
    pub vm_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

/// NetworkModeKind names the workload's relationship to its network ID.
/// Derived from (network_id, validators) — there is no flag field, no
/// `parent`, no `sovereign: bool`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, PartialEq, Eq)]
pub enum NetworkModeKind {
    /// Hosted on a Lux primary, sharing its validator set.
    /// network_id ∈ {1,2,3,1337} AND validators == 0.
    L2,
    /// Runs its own validator subset against a Lux primary. Covers both
    /// the primary itself and any sovereign L1 anchored to it.
    /// network_id ∈ {1,2,3,1337} AND validators > 0.
    Anchored,
    /// Own primary, fully independent of Lux.
    /// network_id ∉ {1,2,3,1337} AND validators > 0.
    Independent,
}

/// Reserved primary network IDs.
pub const PRIMARY_NETWORK_ID_MAINNET: u32 = 1;
pub const PRIMARY_NETWORK_ID_TESTNET: u32 = 2;
pub const PRIMARY_NETWORK_ID_DEVNET: u32 = 3;
pub const PRIMARY_NETWORK_ID_LOCALNET: u32 = 1337;

/// True iff nid is one of {1, 2, 3, 1337}.
pub fn is_primary_network_id(nid: u32) -> bool {
    matches!(
        nid,
        PRIMARY_NETWORK_ID_MAINNET
            | PRIMARY_NETWORK_ID_TESTNET
            | PRIMARY_NETWORK_ID_DEVNET
            | PRIMARY_NETWORK_ID_LOCALNET
    )
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Network",
    plural = "networks",
    namespaced,
    status = "NetworkStatus",
    shortname = "hnet"
)]
/// NetworkSpec — unified blockchain-network CRD.
///
/// Two data fields drive everything: network_id + validators. Mode is
/// derived (see NetworkSpec::network_mode); there is no `sovereign`
/// flag, no `parent` pointer, no `mode` enum field.
#[serde(rename_all = "camelCase")]
pub struct NetworkSpec {
    /// What network this instance is on / part of. Matches luxd's
    /// LUX_NETWORK_ID env var. Reserved values {1,2,3,1337} denote Lux
    /// primaries; any other value denotes an independent primary's own ID.
    #[serde(rename = "networkID")]
    pub network_id: u32,

    /// EVM chain ID (EIP-155 replay-protection root). Unique per
    /// brand × env across the canonical map at
    /// luxfi/genesis/configs/lp182_chain_id_map.go.
    #[serde(rename = "evmChainID", default)]
    pub evm_chain_id: u64,

    /// Validator-set size declaration.
    ///   0 → this CR emits no validator workloads. Listed chains are
    ///        served by the existing validator set on the network
    ///        identified by network_id (L2 mode).
    ///   N → this CR emits N validator pods that participate in the
    ///        network identified by network_id (Anchored or
    ///        Independent mode depending on network_id).
    #[serde(default)]
    pub validators: i32,

    /// Blockchains hosted on this network. Opaque to the operator.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainRef>,

    /// Per-validator pod-spec template applied when validators > 0.
    /// Ignored when validators == 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator_template: Option<ValidatorSpec>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<SubServiceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explorer: Option<ExplorerSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<SubServiceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootnode: Option<SubServiceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,
}

impl NetworkSpec {
    /// Derive the workload's mode from (network_id, validators). No
    /// flag dispatch — pure data → mode.
    pub fn network_mode(&self) -> NetworkModeKind {
        if is_primary_network_id(self.network_id) {
            if self.validators > 0 {
                NetworkModeKind::Anchored
            } else {
                NetworkModeKind::L2
            }
        } else {
            NetworkModeKind::Independent
        }
    }

    /// True when this CR emits validator workloads (validators > 0);
    /// false when it borrows the network's existing set.
    pub fn has_own_validator_set(&self) -> bool {
        self.validators > 0
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// Mode derived from spec data — surfaced for kubectl describe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<NetworkModeKind>,
    #[serde(default)]
    pub active_validators: i32,
    #[serde(default)]
    pub bootstrap_complete: bool,
    #[serde(default)]
    pub chain_count: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ============================================================================
// Ingress Kind
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct IngressRoute {
    pub path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path_type: String,
    pub service_name: String,
    pub service_port: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DomainConfig {
    pub domain: String,
    pub routes: Vec<IngressRoute>,
    #[serde(default = "default_true")]
    pub tls: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct IngressDaemonSetSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare_credentials: Option<SecretReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Ingress",
    plural = "ingresses",
    namespaced,
    status = "IngressStatus",
    shortname = "hing"
)]
#[serde(rename_all = "camelCase")]
pub struct IngressKindSpec {
    pub domains: Vec<DomainConfig>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ingress_class_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cluster_issuer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_daemon_set: Option<IngressDaemonSetSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CertificateStatus {
    pub domain: String,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct IngressStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub managed_ingresses: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub certificate_statuses: Vec<CertificateStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ============================================================================
// DNS Kind
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DNSZoneSpec {
    pub name: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        rename = "cloudflareZoneId"
    )]
    pub cloudflare_zone_id: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CoreDNSSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    #[serde(default)]
    pub api_port: i32,
    #[serde(default)]
    pub dns_port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct CloudflareSyncSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_ref: Option<SecretReference>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct DatabaseSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_ref: Option<SecretReference>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_interval: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct OIDCSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub issuer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub audience: String,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "DNS",
    plural = "dns",
    namespaced,
    status = "DNSStatus",
    shortname = "hdns"
)]
#[serde(rename_all = "camelCase")]
pub struct DNSSpec {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zones: Vec<DNSZoneSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coredns: Option<CoreDNSSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareSyncSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<DatabaseSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<OIDCSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ZoneSyncStatus {
    pub name: String,
    pub coredns_synced: bool,
    pub cloudflare_synced: bool,
    #[serde(default)]
    pub record_count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_time: Option<Time>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct DNSStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub managed_zones: i32,
    #[serde(default)]
    pub coredns_ready: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub zone_statuses: Vec<ZoneSyncStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ============================================================================
// Base Kind — hanzoai/base-ha cluster (Hanzo Base, IAM-native). Bare-named
// `Base` (plural `bases`, singular `base`, shortname `bapp`); exposed under
// the configured white-label group (default `hanzo.ai`).
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BaseGatewaySpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub route: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gateway_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub gateway_namespace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub leader_poll_interval: String,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        rename = "readYourWritesTTL"
    )]
    pub read_your_writes_ttl: String,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Base",
    plural = "bases",
    singular = "base",
    namespaced,
    status = "BaseStatus",
    shortname = "bapp"
)]
#[serde(rename_all = "camelCase")]
pub struct BaseSpec {
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    #[serde(default)]
    pub port: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub consensus: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schema: String,
    pub storage: StorageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<EnvFromSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<LocalObjectReference>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub service_account_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<BaseGatewaySpec>,
    #[serde(default, skip_serializing_if = "String::is_empty", rename = "iamApp")]
    pub iam_app: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kms_secrets: Vec<KMSSecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_policy: Option<NetworkPolicySpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_monitor: Option<ServiceMonitorSpec>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub part_of: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BaseStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub current_writer: String,
    #[serde(default)]
    pub term: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ============================================================================
// New unbranded facades.
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "SQL",
    plural = "sqls",
    namespaced,
    status = "DatastoreStatus",
    shortname = "sql"
)]
#[serde(rename_all = "camelCase")]
pub struct SQLSpec(pub DBSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "KV",
    plural = "kvs",
    namespaced,
    status = "DatastoreStatus",
    shortname = "kv"
)]
#[serde(rename_all = "camelCase")]
pub struct KVSpec(pub DBSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "DocDB",
    plural = "docdbs",
    namespaced,
    status = "DatastoreStatus",
    shortname = "docdb"
)]
// DocDB composes SQL: FerretDB speaks the MongoDB wire protocol OVER Postgres,
// so a DocDB value IS a SQL value shape (`DocDBSpec(SQLSpec(DBSpec))`). The
// controller unwraps to the shared `DBSpec` and materializes the single
// `hanzoai/docdb` StatefulSet (FerretDB + its embedded Postgres backend).
#[serde(rename_all = "camelCase")]
pub struct DocDBSpec(pub SQLSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "IAM",
    plural = "iams",
    namespaced,
    status = "ServiceStatus",
    shortname = "iam"
)]
#[serde(rename_all = "camelCase")]
pub struct IAMSpec(pub ServiceSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "KMS",
    plural = "kmsapps",
    namespaced,
    status = "ServiceStatus",
    shortname = "kms"
)]
#[serde(rename_all = "camelCase")]
pub struct KMSSpec(pub ServiceSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "LLM",
    plural = "llms",
    namespaced,
    status = "ServiceStatus",
    shortname = "llm"
)]
#[serde(rename_all = "camelCase")]
pub struct LLMSpec(pub ServiceSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "S3",
    plural = "s3s",
    namespaced,
    status = "DatastoreStatus",
    shortname = "s3"
)]
#[serde(rename_all = "camelCase")]
pub struct S3Spec(pub DBSpec);

/// Per-tenant isolated database. Paid/isolated tenants get a dedicated
/// `Datastore`-family workload (StatefulSet + Service + headless + PVC). Unlike
/// the `SQL`/`KV`/`DocDB`/`S3` facades — whose engine is fixed by their Kind —
/// `ManagedDatabase` lets the tenant pick the engine at runtime, so it is the
/// one Kind that carries an explicit `engine` selector alongside the shared
/// workload spec. Reconciled by `controllers::managed_database`.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "ManagedDatabase",
    plural = "manageddatabases",
    namespaced,
    status = "DatastoreStatus",
    shortname = "mdb"
)]
#[serde(rename_all = "camelCase")]
pub struct ManagedDatabaseSpec {
    /// Tenant-selected engine. The other datastore Kinds fix their engine by
    /// Kind; this is the one Kind whose engine is a first-class value.
    #[serde(default)]
    pub engine: Engine,
    /// The shared datastore workload spec — flattened so a `ManagedDatabase`
    /// CR's `spec` carries the `storage`/`image`/… fields at top level, exactly
    /// as the `Datastore` family does.
    #[serde(flatten)]
    pub db: DBSpec,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Chain",
    plural = "chains",
    namespaced,
    status = "NetworkStatus",
    shortname = "chain"
)]
#[serde(rename_all = "camelCase")]
pub struct ChainKindSpec {
    pub network: String,
    pub chain: ChainSpec,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Validator",
    plural = "validators",
    namespaced,
    status = "ServiceStatus",
    shortname = "val"
)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorKindSpec {
    pub network: String,
    pub spec: ValidatorSpec,
}

// Standalone Network facade (formerly a sub-resource) is
// dropped. The canonical `Network` kind is the sovereign-L1 CRD above
// (line ~645) — it owns chains directly. There is no separate
// chain-owner Network kind in this operator.

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Indexer",
    plural = "indexers",
    namespaced,
    status = "ServiceStatus",
    shortname = "idx"
)]
#[serde(rename_all = "camelCase")]
pub struct IndexerKindSpec(pub ServiceSpec);

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Explorer",
    plural = "explorers",
    namespaced,
    status = "ServiceStatus",
    shortname = "exp"
)]
#[serde(rename_all = "camelCase")]
pub struct ExplorerKindSpec(pub ServiceSpec);

// ============================================================================
// SPA Kind — standalone hanzoai/spa runtime, per-site pod
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SPASpecInner {
    pub runtime: ImageSpec,
    pub content: ImageSpec,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
    #[serde(default = "default_replicas")]
    pub replicas: i32,
    #[serde(default)]
    pub multi_app: bool,
    #[serde(default)]
    pub ingress: Option<IngressSpec>,
    #[serde(default)]
    pub pdb: Option<PodDisruptionBudgetSpec>,
    #[serde(default)]
    pub resources: Option<ResourceRequirements>,
}

fn default_replicas() -> i32 {
    1
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "SPA",
    plural = "spas",
    namespaced,
    status = "ServiceStatus",
    shortname = "spa"
)]
#[serde(rename_all = "camelCase")]
pub struct SPAKindSpec(pub SPASpecInner);

// ============================================================================
// Static Kind — hanzoai/static ingress plugin, NO separate pod
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct StaticSpecInner {
    /// ConfigMap name containing site files
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub config_map: String,
    /// OR OCI image + path to extract content from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSpec>,
    pub ingress: IngressSpec,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Static",
    plural = "statics",
    namespaced,
    status = "ServiceStatus",
    shortname = "static"
)]
#[serde(rename_all = "camelCase")]
pub struct StaticKindSpec(pub StaticSpecInner);

// ============================================================================
// Queue Kind — message broker (NATS, Kafka, JetStream)
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Queue",
    plural = "queues",
    namespaced,
    status = "ServiceStatus",
    shortname = "q"
)]
#[serde(rename_all = "camelCase")]
pub struct QueueKindSpec(pub ServiceSpec);

// ============================================================================
// Observability Kind — Grafana / OTEL Collector / VictoriaMetrics
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Observability",
    plural = "observabilities",
    namespaced,
    status = "ServiceStatus",
    shortname = "obs"
)]
#[serde(rename_all = "camelCase")]
pub struct ObservabilityKindSpec(pub ServiceSpec);

// ============================================================================
// Function Kind — OpenFaaS / Knative serverless function
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Function",
    plural = "functions",
    namespaced,
    status = "ServiceStatus",
    shortname = "fn"
)]
#[serde(rename_all = "camelCase")]
pub struct FunctionKindSpec(pub ServiceSpec);

// ============================================================================
// LuxRuntime Kind — luxd validator-set deployment (mirrors Go api/v1
// luxruntime_types.go field shapes). Canonical Kind name at `bootno.de/v1`
// is `LuxRuntime` (plural `luxruntimes`, shortname `lrt`); the same Kind is
// exposed here under the configured white-label group (default `hanzo.ai`).
// ============================================================================

/// One seed-restore transport. The init container walks `sources` in order
/// and uses the first that succeeds.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SeedSource {
    /// Transport class: `ObjectStore` | `InternalHTTP` | `OCIArtifact` | `PeerPod`.
    #[serde(rename = "type")]
    pub type_: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub expected_hash: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SeedRestoreSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SeedSource>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data_dir: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct WipeOnRecreateSpec {
    /// `none` | `fullDB` | `chainData/<chainID>`. Default `none`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scope: String,
}

/// Configures native ZAP replication of the node's ZapDB to an object store
/// (hanzoai/vfs `s3://`). When enabled, the operator emits the full native
/// pipeline as `REPLICATE_*` env: CDC change-feed incrementals (no keyspace
/// scan), physical SST-copy snapshots, per-DB streams, restore-on-boot, and
/// post-quantum (ML-KEM-768) encryption client-side. A single ordinal writes
/// the shared stream (`sourceNodeIndex`); every peer restores-on-boot only.
///
/// Mirrors Go `api/v1` `ReplicationSpec` field-for-field.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ReplicationSpec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_endpoint: String,
    /// S3 bucket for replication objects. Default `replicate`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_bucket: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_region: String,
    /// S3 key prefix; defaults to the node's db path.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub s3_path: String,
    #[serde(rename = "s3UseSsl", default)]
    pub s3_use_ssl: bool,
    /// K8s Secret with `REPLICATE_S3_ACCESS_KEY` / `_SECRET_KEY`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credentials_secret: String,
    /// age public key (`age1pq1...` for post-quantum) enabling client-side
    /// encryption of snapshots.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub age_recipient: String,
    /// K8s Secret holding `REPLICATE_AGE_IDENTITY` for restore.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub age_identity_secret: String,
    /// Only this ordinal writes; peers restore-on-boot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_node_index: Option<i32>,
    /// Seconds between full snapshots. Default 3600.
    #[serde(default)]
    pub snapshot_interval_seconds: i64,
    /// Seconds between incrementals. Default 5.
    #[serde(default)]
    pub incremental_interval_seconds: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LuxChainSpec {
    #[serde(rename = "chainID")]
    pub chain_id: String,
    #[serde(rename = "vmID", default, skip_serializing_if = "String::is_empty")]
    pub vm_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub genesis_config_map: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_blocking: Option<bool>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub component: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VMPluginRef {
    #[serde(rename = "vmID")]
    pub vm_id: String,
    pub object_key: String,
    #[serde(rename = "chainIDs", default, skip_serializing_if = "Vec::is_empty")]
    pub chain_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PluginSourceSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bucket: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub plugin_dir: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vm_plugins: Vec<VMPluginRef>,
}

/// One-time RLP import for a tenant chain.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenantImportSpec {
    pub tenant: String,
    pub chain_alias: String,
    #[serde(rename = "sourceURL")]
    pub source_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    #[serde(rename = "blockchainID")]
    pub blockchain_id: String,
}

/// In-namespace ConfigMap pointer.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapReference {
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenantChainTrack {
    #[serde(rename = "blockchainId")]
    pub blockchain_id: String,
    #[serde(rename = "vmId")]
    pub vm_id: String,
    pub config_map_ref: ConfigMapReference,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TenantNetworkImport {
    #[serde(rename = "parentNetworkId")]
    pub parent_network_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<TenantChainTrack>,
}

/// In-cluster PVC destination for a chain-state export.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportDestinationPVC {
    pub claim_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub_path: String,
}

/// S3-compatible bucket destination for a chain-state export.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportDestinationS3 {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_prefix: String,
    pub credentials_secret_ref: LocalObjectReference,
}

/// Discriminated union — exactly one of `pvc` / `s3` is set.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExportDestination {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pvc: Option<ExportDestinationPVC>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<ExportDestinationS3>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExportScheduleSpec {
    pub name: String,
    pub chain_alias: String,
    pub schedule: String,
    #[serde(default)]
    pub from_height: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub to_height: String,
    pub destination: ExportDestination,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ExportScheduleStatus {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_schedule_time: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_time: Option<Time>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_error: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct TenantNetworkStatus {
    #[serde(rename = "parentNetworkId")]
    pub parent_network_id: String,
    #[serde(rename = "blockchainId")]
    pub blockchain_id: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChainStatus {
    pub alias: String,
    #[serde(
        rename = "blockchainId",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub blockchain_id: String,
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_observed: Option<Time>,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "LuxRuntime",
    plural = "luxruntimes",
    namespaced,
    status = "LuxRuntimeStatus",
    shortname = "lrt",
    printcolumn = r#"{"name":"NetworkID","type":"integer","jsonPath":".spec.networkID"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Validators","type":"integer","jsonPath":".status.activeValidators"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct LuxRuntimeSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub network_name: String,
    #[serde(rename = "networkID", default)]
    pub network_id: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validators: Option<i32>,
    #[serde(default)]
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<LuxChainSpec>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub genesis_config_map: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_source: Option<PluginSourceSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenant_imports: Vec<TenantImportSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export_schedules: Vec<ExportScheduleSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenant_networks: Vec<TenantNetworkImport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_restore: Option<SeedRestoreSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wipe_on_recreate: Option<WipeOnRecreateSpec>,
    #[serde(default)]
    pub staking_port: i32,
    #[serde(default)]
    pub http_port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,

    /// Turns on native ZAP replication: the node streams CDC incrementals +
    /// physical snapshots to S3 (database >= v1.20.3) and restores-on-boot.
    /// Translated to `REPLICATE_*` env on the luxd container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication: Option<ReplicationSpec>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct LuxRuntimeStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub active_validators: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tenant_networks: Vec<TenantNetworkStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub export_schedules: Vec<ExportScheduleStatus>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chain_statuses: Vec<ChainStatus>,
}

// ============================================================================
// NodeFleet Kind — "1 archive serves N state-sync replicas" topology
// (mirrors Go api/v1 nodefleet_types.go field shapes).
// ============================================================================

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct AncientStoreSpec {
    /// Freezer backend: `zap` (canonical) | `legacy`. Default `zap`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub backend: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default)]
    pub max_table_size: i64,
}

/// Minimal nodeAffinity subset NodeFleet composes onto pods.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct HostAffinitySpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancient_store: Option<AncientStoreSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_affinity: Option<HostAffinitySpec>,
    #[serde(rename = "snapshotCacheMB", default)]
    pub snapshot_cache_mb: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct StateSyncSpec {
    pub replicas: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_affinity: Option<HostAffinitySpec>,
    #[serde(default)]
    pub state_sync_min_blocks: i32,
    #[serde(rename = "snapshotCacheMB", default)]
    pub snapshot_cache_mb: i32,
    #[serde(rename = "blockCacheMB", default)]
    pub block_cache_mb: i32,
    #[serde(rename = "trieCacheMB", default)]
    pub trie_cache_mb: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub low_memory: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FleetChainSpec {
    pub alias: String,
    #[serde(rename = "vmID", default, skip_serializing_if = "String::is_empty")]
    pub vm_id: String,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "NodeFleet",
    plural = "nodefleets",
    namespaced,
    status = "NodeFleetStatus",
    shortname = "fleet",
    printcolumn = r#"{"name":"NetworkID","type":"integer","jsonPath":".spec.networkID"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NodeFleetSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub network_name: String,
    #[serde(rename = "networkID", default)]
    pub network_id: i32,
    #[serde(default)]
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<FleetChainSpec>,
    #[serde(default)]
    pub archive: ArchiveSpec,
    #[serde(default)]
    pub state_sync: StateSyncSpec,
    #[serde(default)]
    pub http_port: i32,
    #[serde(default)]
    pub staking_port: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_pull_secrets: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeFleetStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub archive_ready: bool,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ============================================================================
// AgentDeployment Kind — the autonomous-bot lifecycle
// ============================================================================

/// Declarative desired state for a Bot: a cloud Agent (`org/agentName`) run
/// long-running on a visor-provisioned machine bound to the `@hanzo/bot`
/// runtime. The controller converges three facts each reconcile:
///
/// 1. the cloud Agent exists in `/v1/agents` with `execution_mode=long-running`,
/// 2. a visor machine is provisioned and bound to it
///    (`POST /v1/machines/:id/bind-agent`),
/// 3. desired == running (the binding reconciles to `Bound`).
///
/// It reaches TWO external control planes over HTTP (cloud `/v1/agents`, visor
/// `/v1/machines`); like the apps DRIVE controller it is **opt-in and
/// provisioning-gated** because launching a machine costs money — see
/// `controllers::agent_deployment` for the gate model.
///
/// `bot` is a shortname because a Bot is exactly what this Kind materializes.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "AgentDeployment",
    plural = "agentdeployments",
    namespaced,
    status = "AgentDeploymentStatus",
    shortname = "agentdeploy",
    shortname = "bot",
    printcolumn = r#"{"name":"Agent","type":"string","jsonPath":".spec.agentName"}"#,
    printcolumn = r#"{"name":"Org","type":"string","jsonPath":".spec.org"}"#,
    printcolumn = r#"{"name":"Mode","type":"string","jsonPath":".spec.executionMode"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Binding","type":"string","jsonPath":".status.bindingStatus"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AgentDeploymentSpec {
    /// The cloud Agent's name in the `/v1/agents` registry. Combined with `org`
    /// this is the Agent identity (`<org>-<agentName>` service-account naming).
    pub agent_name: String,
    /// The cloud/IAM organization that owns the Agent.
    pub org: String,
    /// Execution mode of the Agent. A Bot is by definition long-running, so this
    /// defaults to `long-running`; the controller ensures the cloud Agent
    /// carries this mode. Kept explicit (not hard-coded) so a future ephemeral
    /// mode is a spec change, not a code change.
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String,
    /// Optional cron schedule for scheduled (non-continuous) execution. Empty ⇒
    /// continuously running. Recorded on the Agent and surfaced in status;
    /// the scheduler that fires runs consumes it (out of this controller's
    /// scope — this controller owns provisioning + binding, not triggering).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub schedule: String,
    /// Desired number of bound machines running this bot. Defaults to 1 (a bot
    /// is normally singleton — one persistent compute holds its brain state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replicas: Option<i32>,
    /// Optional pinned `@hanzo/bot` npm version for the runtime. Empty ⇒ the
    /// machine's launch default (cloud-init installs latest).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bot_version: String,
    /// Optional visor cloud provider to launch the machine on (e.g.
    /// `DigitalOcean`). Empty ⇒ the controller only binds an already-provisioned
    /// machine and never launches one (bind-only, zero-cost).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    /// Optional explicit machine id (`owner/name`) to bind. When set the
    /// controller binds THIS machine and never launches; when empty and a
    /// provider is set + launching is enabled, it launches one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine_id: String,
}

/// Default execution mode — a Bot is long-running by definition.
fn default_execution_mode() -> String {
    "long-running".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentDeploymentStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// True once the cloud Agent exists with the desired execution mode.
    #[serde(default)]
    pub agent_ready: bool,
    /// The visor machine id this bot is bound to, once known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine_id: String,
    /// The visor binding's honest status (`Pending`/`Bound`/`Error`), mirrored
    /// verbatim so `kubectl get bot` shows real convergence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub binding_status: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

// ============================================================================
// App Kind — the ONE Hanzo workload CRD (apps.hanzo.ai). The 29th Kind: a
// role-dispatch super-facade that collapses the former Service/Datastore/…
// Kinds into a single deployable whose `spec.role` selects a reconcile PROFILE
// (a value, not a place). App IS a renamed Service — its workload core is
// literally `ServiceSpec`, flattened — so every field a fleet App CR carries is
// already handled by an existing reconcile. Absent role ⇒ the Service profile
// (the hot path: 62 of the 67 live App CRs carry no role). Role-specific fields
// (a datastore's `storage`/`type`, an ingress's `domains`) are NOT modeled here
// — they ride as preserved unknowns (`x-kubernetes-preserve-unknown-fields`,
// injected on the spec by the CRD generator) and are projected onto the
// delegate spec at dispatch time. Reconciled by `controllers::app`, which
// dispatches on `spec.role` to the existing `reconcile_*_inner_pub` functions.
// ============================================================================

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "App",
    plural = "apps",
    singular = "app",
    namespaced,
    status = "ServiceStatus",
    shortname = "app",
    printcolumn = r#"{"name":"Role","type":"string","jsonPath":".spec.role"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Image","type":"string","jsonPath":".spec.image.repository"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct AppSpec {
    /// The role PROFILE that reconciles this App — the single field that replaced
    /// the ~28 former hanzo.ai/v1 Kinds (values, not places). Absent/`generic`/
    /// `service` and the schema-identical `llm`/`iam`/`kms`/`explorer`/`function`/
    /// `indexer`/`observability`/`queue`/`spa`/`static` select the generic Service
    /// profile; `sql`/`kv`/`docdb`/`s3`/`datastore`/`managedDatabase` the datastore
    /// profile; `dns`/`ingress` their controllers. Every other value delegates to
    /// its dedicated controller (or a NoOp stub). An unrecognized value reconciles
    /// fail-safe (status marks it, requeue — never a delete, never a panic). It is
    /// an OPEN string (not the closed enum) so this fail-safe path is reachable at
    /// runtime rather than rejected at admission; see `controllers::app::classify`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,

    /// The generic workload core — the superset spec every profile draws from.
    /// Flattened so an App CR carries `image`/`env`/`ports`/`persistence`/… at the
    /// top level exactly as the former `Service` Kind did. For a service-role App
    /// this IS the reconcile input verbatim; datastore/dns/ingress roles project
    /// the full spec (this + `extra`) onto their delegate spec at dispatch.
    #[serde(flatten)]
    pub service: ServiceSpec,

    /// Role-specific fields not in the generic core (a datastore's
    /// `storage`/`type`/`credentialsSecret`/`serviceAliases`, an ingress's
    /// `domains`/`clusterIssuer`/`ingressClassName`). Captured verbatim so the
    /// dispatch can project them onto the delegate spec, and PRESERVED end-to-end:
    /// the CRD carries `x-kubernetes-preserve-unknown-fields: true`, so the
    /// apiserver never prunes them. `#[schemars(skip)]` keeps them out of the
    /// structural schema — the preserve-unknown flag is the ONE mechanism that
    /// carries them, matching the merged universe CRD (which models only the
    /// ServiceSpec field set + `role`). This is the exact bug (HIGH-2) that sank
    /// the reduced fork: a modeled-but-unpreserved spec prunes the datastore
    /// fields. Preserve-unknown + this catch-all keep every field.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: BTreeMap<String, serde_json::Value>,
}
