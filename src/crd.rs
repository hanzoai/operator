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
    Condition, Container, EnvFromSource, EnvVar, LocalObjectReference, PodSecurityContext,
    SecretReference, SecurityContext, Time, Toleration, Volume, VolumeMount,
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

/// The class every Ingress we emit falls back to when a CR names none.
///
/// hanzoai/ingress runs with `--providers.kubernetesingress.ingressclass=ingress`,
/// so an Ingress carrying no class matches no provider and builds no router. It
/// is inert, and silently so: the host 404s with router "-" while its Service
/// keeps ready endpoints, which reads as an app bug rather than a routing gap.
/// Defaulting here is what keeps "class omitted" from meaning "dark".
pub const DEFAULT_INGRESS_CLASS: &str = "ingress";

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

/// Wires a node's luxd staking identity — TLS cert, BLS signer and ML-DSA-65 —
/// from KMS rather than from anything checked in. hanzo/cloud's validator
/// onboarding seals the keys under an org-scoped KMS coordinate; declaring `kms`
/// here makes the operator reconcile a KMSSecret so the kms-operator materializes
/// them into `secret_name`, which the pod mounts read-only at /staking-keys.
/// Omit `kms` when the Secret is provisioned out of band.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StakingSpec {
    /// The Secret holding the luxd staking artifacts (staker.crt, staker.key,
    /// signer.key, mldsa.key, mldsa.pub). Defaults to the KMS ref's managed
    /// secret name when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret_name: String,
    /// The KMS -> Secret sync to reconcile. Without it nothing populates
    /// `secret_name`, which is why it is separate from the name itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kms: Option<KMSSecretRef>,
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
    /// SeaweedFS requires path-style addressing (subdomain buckets don't
    /// resolve in-cluster). Default `true`.
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

    // CRITICAL: env/volumes/volumeMounts are contract surface — they must reach
    // the rendered pod template. Dropping one starts a misconfigured workload.
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
    /// Pod-level `securityContext` passthrough — `runAsNonRoot`, `runAsUser`,
    /// `runAsGroup`, `fsGroup`, `seccompProfile` — rendered onto the PodSpec's
    /// `securityContext`. The legacy top-level `fsGroup` above folds together
    /// with this: `securityContext.fsGroup` wins when both are set, otherwise the
    /// top-level value is carried. Opt-in — a CR that omits it renders a
    /// byte-identical PodSpec (a CR that sets only the legacy `fsGroup` still
    /// renders exactly `securityContext: {fsGroup: N}` as before).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<PodSecurityContext>,
    /// Container-level `securityContext` for the MAIN container —
    /// `readOnlyRootFilesystem`, `allowPrivilegeEscalation`,
    /// `capabilities{drop,add}`, `runAsNonRoot`, `runAsUser` — rendered onto the
    /// main container's `securityContext`. This is the fleet hardening baseline
    /// (`readOnlyRootFilesystem: true` + `capabilities.drop: [ALL]` +
    /// `allowPrivilegeEscalation: false`) that the LLM-key-holding and
    /// cluster-admin workloads set; porting them without it silently DROPS the
    /// hardening. Opt-in — an omitting CR renders a byte-identical container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_security_context: Option<SecurityContext>,
    /// `enableServiceLinks` on the PodSpec. The object store (`s3`/SeaweedFS)
    /// MUST set this `false`: k8s's default-`true` injects a
    /// `*_SERVICE_HOST/PORT` env var for every Service in the namespace, and the
    /// s3 flag parser aborts startup on the unexpected vars. `None` ⇒ the field
    /// is omitted (k8s default `true`), byte-identical for every CR that does not
    /// set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_service_links: Option<bool>,
    /// Managed-upgrade policy. OPT-IN: when absent, `spec.image` is applied
    /// directly (historical behavior). When `enabled` (+ the operator's
    /// `UPGRADE_FSM_ENABLED` gate), an image change rolls through the
    /// pre-flight → health-gate → auto-rollback FSM (see `controllers::upgrade`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade_policy: Option<UpgradePolicySpec>,

    // ── Placement ────────────────────────────────────────────────────────────
    // WHY THESE EXIST. Without them the spec could express what a workload NEEDS
    // (`resources`) but not WHERE it must go, so an App that required a specific
    // node had exactly one lever: inflate `resources.requests` until only that
    // node could fit it. crs/cloud.yaml did precisely that — a 9Gi request whose
    // stated purpose was "force the scheduler to place cloud [on worker-xl] (the
    // App CRD cannot express nodeSelector/tolerations)".
    //
    // That idiom is not a reservation, it is a RACE. `strategy: Recreate`
    // deletes the pod before its replacement is scheduled, and a request
    // reserves nothing while the pod is gone: any neighbour may take the only
    // node large enough, after which the inflated pod fits NOWHERE and the
    // scheduler is deadlocked until something is moved by hand.
    //
    // So: resources say what it NEEDS, placement says WHERE it goes, and
    // `priorityClassName` is what turns the seat into an actual reservation —
    // a preempting pod evicts a squatter instead of queueing behind it.
    //
    // All three are Option/empty-default with `skip_serializing_if`, and each
    // renders `None` on the PodSpec when unset, so every CR that omits them
    // reconciles to a byte-identical Deployment (the same additive contract the
    // securityContext passthrough holds to).
    /// `PodSpec.nodeSelector` — the hard "only nodes with these labels" filter.
    /// Prefer this over an inflated `resources.requests` when the intent is
    /// placement: it says so out loud, and it keeps the request honest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// `PodSpec.tolerations` — lets this workload onto tainted nodes. The
    /// companion to `nodeSelector` for a DEDICATED pool: taint the pool so
    /// nothing else lands there, then tolerate the taint here. That pair is a
    /// real reservation; a big memory request is not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<Toleration>,

    /// `PodSpec.priorityClassName` — the name of an existing PriorityClass.
    /// This is the field that makes a seat survive the `Recreate` window: a
    /// higher-priority pod PREEMPTS a lower-priority squatter rather than going
    /// Pending behind it. The referenced PriorityClass is cluster-scoped and is
    /// NOT created here — naming one that does not exist makes the apiserver
    /// reject the pod, so it fails loudly rather than silently placing wrong.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub priority_class_name: String,
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
    /// SeaweedFS object store (`hanzoai/s3`).
    S3,
    /// hanzoai/datastore analytics engine (Hanzo Datastore).
    Datastore,
}

impl Engine {
    /// Canonical identity string — the `app.kubernetes.io/component` label and
    /// the default service-port name. Carried over from the retired `spec.type`
    /// discriminator so an adopted StatefulSet's pod spec stays byte-identical
    /// (no needless rollout).
    ///
    /// The object store is the one that moved. Its component label now reads
    /// `s3`, naming what actually runs: SeaweedFS, our own `hanzoai/s3`. The
    /// move was free — no live resource carried the old label. Keep this
    /// string a description of the engine we run, never of an upstream.
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgresql",
            Engine::Valkey => "valkey",
            Engine::Docdb => "docdb",
            Engine::S3 => "s3",
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
    /// (e.g. Hanzo Datastore `datastore`), which already write any volume. Opt-in so
    /// changing it never restarts unrelated datastores (a datastore that leaves
    /// it None gets a byte-identical StatefulSet).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs_group: Option<i64>,
    /// Pod-level `securityContext` passthrough — `runAsNonRoot`, `runAsUser`,
    /// `runAsGroup`, `fsGroup`, `seccompProfile` — rendered onto the
    /// StatefulSet PodSpec's `securityContext`. The legacy top-level `fsGroup`
    /// above folds together with this: `securityContext.fsGroup` wins when both
    /// are set, otherwise the top-level value is carried. Opt-in — a datastore
    /// that omits it renders a byte-identical StatefulSet (one that sets only the
    /// legacy `fsGroup` still renders exactly `securityContext: {fsGroup: N}` as
    /// before). Symmetric with the Service path (`ServiceSpec.securityContext`)
    /// so a hardened workload ported to a datastore role keeps its posture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_context: Option<PodSecurityContext>,
    /// Container-level `securityContext` for the MAIN engine container —
    /// `readOnlyRootFilesystem`, `allowPrivilegeEscalation`,
    /// `capabilities{drop,add}`, `runAsNonRoot`, `runAsUser` — rendered onto the
    /// engine container's `securityContext` ONLY (never the replication sidecar,
    /// which must keep a writable rootfs to stream the WAL). Opt-in — an omitting
    /// datastore renders a byte-identical container. Symmetric with the Service
    /// path (`ServiceSpec.containerSecurityContext`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_security_context: Option<SecurityContext>,
    /// `enableServiceLinks` on the StatefulSet PodSpec. The object store
    /// (`s3`/SeaweedFS) MUST set this `false`: k8s's default-`true` injects a
    /// `*_SERVICE_HOST/PORT` env var for every Service in the namespace, and the
    /// s3 flag parser aborts startup on the unexpected vars. This is the field
    /// the datastore path uniquely needs (the object store is a datastore role);
    /// `None` ⇒ the field is omitted (k8s default `true`), byte-identical for
    /// every datastore that does not set it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_service_links: Option<bool>,
}

/// `Datastore` Kind — the `hanzoai/datastore` analytics engine (Hanzo Datastore).
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

/// A secret this cluster needs, and where in KMS it comes from.
///
/// The operator both WRITES these (from a `kmsSecrets` reference on a Service,
/// App or LuxRuntime) and READS them (`controllers::kms` projects them into
/// k8s Secrets). Owning the definition is what lets those two agree:
/// while the CRD lived in another repo, the writer and the projector drifted to
/// different groups and versions and nothing said so.
///
/// The group is `kms.<universe>` — prefixed, so it is rewritten to
/// `kms.lux.cloud` under lux while the universe's own Kinds go to `lux.cloud`.
/// KMS is named for the service that owns it, not the universe it serves.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "kms.hanzo.ai",
    version = "v1",
    kind = "KMSSecret",
    plural = "kmssecrets",
    namespaced,
    shortname = "kmss"
)]
#[serde(rename_all = "camelCase")]
pub struct KMSSecretSpec {
    /// How the operator reaches KMS. `iam` reads KMS over HTTP as the
    /// operator's own IAM client; `zap` selects the ZAP-native path. Anything
    /// else is left to whatever else is watching, so a cluster can be migrated
    /// one secret at a time rather than all at once.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,
    /// host:port of the KMS ZAP endpoint. `zap` only: on `iam` the host is the
    /// operator's own `KMS_URL`, since a CR naming it would aim the operator's
    /// bearer at a host of the CR author's choosing.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub zap_addr: String,
    /// The KMS coordinate: which project, which environment, which folder.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project_slug: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub env_slug: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secrets_path: String,
    /// The keys to fetch. Empty means none — never "all", because a widening
    /// secret is not something a reconcile should decide on its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// The k8s Secret to write.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub managed_secret_name: String,
    /// Namespaces the Secret may be projected into besides the CR's own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_namespaces: Vec<String>,
    /// Restricts this CR to one cluster when several watch the same KMS.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cluster_name: String,
    /// The Secret's `type`. Empty means `Opaque`.
    ///
    /// It has to be declared because the kubelet TYPE-CHECKS a pull secret: it
    /// skips an Opaque one without logging a word, which reads back as a bad
    /// credential when the credential was never consulted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret_type: String,
    /// `Orphan` (the default) leaves the Secret when the CR goes; `Owner`
    /// garbage-collects it.
    ///
    /// Orphan is the default because deleting a reference must not pull env out
    /// from under a running pod. Owner is the right choice only where the
    /// consumer re-reads on restart and a stale value is worse than a missing
    /// one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub creation_policy: String,
    /// Project into this namespace instead of the CR's own.
    ///
    /// For a namespace isolated on purpose, which therefore holds no KMS
    /// identity of its own: the CR lives with its credential and writes the
    /// result across. Requires `Orphan` — an owner reference cannot cross a
    /// namespace, so an owned Secret there would be collected immediately.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub secret_namespace: String,
    /// Rename on the way out: Secret key ← KMS key.
    ///
    /// KMS names a value one thing and the consumer reads another, and this is
    /// where the two spellings meet. It was a Go template doing nothing but a
    /// rename, so it is spelled as one — a template engine here would be a
    /// language to learn, a way to fail at runtime, and no more expressive than
    /// the map for anything anyone actually wrote.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rename: BTreeMap<String, String>,
    /// Constants written into the Secret beside the fetched values.
    ///
    /// A repository credential is a KMS token plus the host, the user and the
    /// kind of repo it is — facts that are not secret and have nowhere else to
    /// live. Kept separate from `rename` because they are different things: one
    /// says where a value comes from, the other IS the value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub literals: BTreeMap<String, String>,
    /// Seconds between refetches. Zero means the projector's own cadence.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub resync_interval: i64,
    /// The KMS machine identity this CR must read as.
    ///
    /// On the legacy path the identity chose the TENANT — the org rode the
    /// minted token, not the path, so the same path string read different
    /// material per identity. The ZAP path authenticates once as a peer derived
    /// from `clusterName`, which identifies the connection and carries no
    /// principal, so it cannot read as someone else.
    ///
    /// A CR that names one is therefore REFUSED rather than served. Serving it
    /// would return a different org's secrets through a path that looks
    /// correct, and nothing downstream could tell. Honouring it needs an
    /// identity on the wire in luxfi/kms.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub credentials_ref: String,
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
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
    group = "bootno.de",
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
    group = "bootno.de",
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
    /// Per-domain annotations, merged on top of the CR-level `spec.annotations`
    /// onto this domain's generated Ingress ONLY. This is the edge-behavior seam:
    /// a host can carry its own Traefik middleware chain
    /// (`traefik.ingress.kubernetes.io/router.middlewares`), a redirect, an
    /// auth-guard, or a rate-limit without affecting sibling domains — so the
    /// declarative aggregator expresses per-host edge behavior the flat
    /// CR-level annotations could not (which applied uniformly to every domain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<BTreeMap<String, String>>,
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

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "bootno.de",
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
    group = "bootno.de",
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
    group = "bootno.de",
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
    group = "bootno.de",
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
    group = "bootno.de",
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
    /// Which fork of the node binary this runtime runs: `luxd`, `hanzod`,
    /// `zood`. Empty means `luxd`.
    ///
    /// These three are one program under three brands — same flags, same
    /// ports, same disk layout — so one Kind with the fork as a VALUE is
    /// right, and three Kinds differing only in an image would be the same
    /// type written out three times.
    ///
    /// A chain of a different SHAPE does not belong here. bitcoind has no
    /// networkID and no validator set; an Ethereum node is two processes
    /// sharing a JWT; a Solana validator votes with a keypair. Each gets its
    /// own Kind below, with its own real fields. The rule is the honest one:
    /// same shape, one Kind and a value; different shape, different Kind.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub engine: String,
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

    /// Staking identity for this runtime, sourced from KMS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staking: Option<StakingSpec>,
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
    group = "bootno.de",
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
    /// the ~28 former hanzo.ai/v1 Kinds (values, not places).
    ///
    /// The values and the profile each one resolves to are one table,
    /// `controllers::app::ROLES`; the enum this CRD publishes is projected from
    /// it, so what the schema accepts is exactly what something reconciles.
    ///
    /// Typed as `String` rather than a Rust enum because the schema is injected
    /// at install time, and because an App created against an older CRD can
    /// carry a role this build has since dropped. `classify` answers Unknown for
    /// anything it does not recognize, and Unknown reconciles fail-safe — status
    /// marks it and it requeues, never a delete, never a panic.
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
    /// ServiceSpec field set + `role`). Modeled-but-unpreserved is the trap: it
    /// looks correct in the Rust type and the apiserver prunes the datastore
    /// fields. Preserve-unknown + this catch-all keep every field.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Native GitOps — the two concerns that retire the push-based hack (the
// `gitops-reconcile` shell cron + the `notify-universe` GitHub dispatch). Both
// are ordinary hanzo.ai CRDs reconciled by this same operator, so there is ONE
// reconciler and ONE api group — no second control plane (ArgoCD was torn out
// on purpose; re-adding it would be a parallel reconciler). The reconcile
// algorithms are the proven GitOps-toolkit ones (pull-sync; semver image
// selection with git write-back), reimplemented natively over the operator's
// existing apply/status machinery — not a vendored platform.
// ─────────────────────────────────────────────────────────────────────────────


// ============================================================================
// Foreign chain runtimes — bitcoind, Ethereum, Solana
//
// LuxRuntime covers the luxd family, where one shape serves three brands. A
// chain outside that family is not a luxd with different flags: bitcoind has
// no validator set and prunes by megabytes; an Ethereum node is two processes
// that authenticate to each other over a shared JWT; a Solana validator votes
// with a keypair and fetches snapshots to catch up. Giving each its own Kind
// costs three small types and buys a schema that can actually reject a wrong
// spec — `txIndex` with `pruning`, a consensus client with no JWT — instead of
// an opaque map the API server waves through and a reconciler discovers at
// runtime.
//
// These mirror github.com/bootnode/operator api/v1 field for field. The
// reconcilers there read these CRs; if the schemas drift, the API server
// accepts specs the reconciler cannot honor, and the CR just sits there.
// ============================================================================

/// A Secret in the CR's namespace, optionally one key inside it.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecretRef {
    pub name: String,
    /// Empty means the whole Secret; builders that project one value default
    /// the key per call site.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
}

// ---------------------------------------------------------------- bitcoin ---

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BitcoinNetwork {
    #[default]
    Mainnet,
    Testnet,
    Regtest,
    Signet,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BitcoinIndexerKind {
    #[default]
    Electrs,
    Esplora,
    Fulcrum,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BitcoinP2PSpec {
    #[serde(default)]
    pub listen_port: i32,
    #[serde(default)]
    pub max_connections: i32,
    #[serde(default)]
    pub add_nodes: Vec<String>,
}

/// An Electrum-protocol sidecar. Absent means no indexer.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BitcoinIndexerSpec {
    pub kind: BitcoinIndexerKind,
    pub image: ImageSpec,
    #[serde(default)]
    pub rpc_port: i32,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "bootno.de",
    version = "v1",
    kind = "BitcoinRuntime",
    plural = "bitcoinruntimes",
    namespaced,
    status = "BitcoinRuntimeStatus",
    shortname = "btcrt"
)]
#[serde(rename_all = "camelCase")]
pub struct BitcoinRuntimeSpec {
    /// The bitcoind image.
    #[serde(default)]
    pub node_image: ImageSpec,
    pub network: BitcoinNetwork,
    /// `prune=` target in MiB. 0 keeps a full archive. Mutually exclusive with
    /// `txIndex` — an index cannot be built over blocks that were discarded.
    #[serde(default)]
    pub pruning: i32,
    /// `txindex=1`. An Electrum indexer needs it to backfill from genesis.
    #[serde(default)]
    pub tx_index: bool,
    /// RPC credentials. Absent means bitcoind's own cookie file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_auth: Option<SecretRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2p: Option<BitcoinP2PSpec>,
    /// PVC template for the data dir.
    pub storage: StorageSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexer: Option<BitcoinIndexerSpec>,
    /// JSON-RPC port. Zero resolves from the network: 8332 / 18332 / 18443 / 38332.
    #[serde(default)]
    pub rpc_port: i32,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct BitcoinRuntimeStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub block_height: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_progress: String,
    #[serde(default)]
    pub peer_count: i32,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// --------------------------------------------------------------- ethereum ---

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EthereumNetwork {
    #[default]
    Mainnet,
    Sepolia,
    Holesky,
    Custom,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionKind {
    #[default]
    Geth,
    Reth,
    Erigon,
    Nethermind,
    Besu,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ConsensusKind {
    #[default]
    Lighthouse,
    Prysm,
    Teku,
    Nimbus,
    Lodestar,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EthereumExecutionSpec {
    pub kind: ExecutionKind,
    pub image: ImageSpec,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_mode: String,
    pub storage: StorageSpec,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EthereumConsensusSpec {
    pub kind: ConsensusKind,
    pub image: ImageSpec,
    /// URL to sync the beacon state from instead of walking the chain.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub checkpoint_sync: String,
    pub storage: StorageSpec,
    #[serde(default)]
    pub extra_args: Vec<String>,
}

/// A post-merge Ethereum node: an execution client and a consensus client,
/// side by side, authenticating to each other over a shared JWT. Neither half
/// syncs alone, which is why they are one CR and not two.
#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "bootno.de",
    version = "v1",
    kind = "EthereumRuntime",
    plural = "ethereumruntimes",
    namespaced,
    status = "EthereumRuntimeStatus",
    shortname = "ethrt"
)]
#[serde(rename_all = "camelCase")]
pub struct EthereumRuntimeSpec {
    pub network: EthereumNetwork,
    pub execution: EthereumExecutionSpec,
    pub consensus: EthereumConsensusSpec,
    /// The shared secret the two halves authenticate the Engine API with.
    pub jwt_secret: SecretRef,
    /// Address credited with block-proposal fees.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fee_recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default, rename = "authRpcPort")]
    pub auth_rpc_port: i32,
    #[serde(default, rename = "elRpcPort")]
    pub el_rpc_port: i32,
    #[serde(default, rename = "clRpcPort")]
    pub cl_rpc_port: i32,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EthereumRuntimeStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    /// Both halves report separately: an execution client can be synced while
    /// the beacon node is still backfilling, and the node serves neither.
    #[serde(default, rename = "elReady")]
    pub el_ready: bool,
    #[serde(default, rename = "clReady")]
    pub cl_ready: bool,
    #[serde(default)]
    pub slot_height: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_progress: String,
    #[serde(default)]
    pub peer_count: i32,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}

// ----------------------------------------------------------------- solana ---

#[derive(Serialize, Deserialize, Clone, Copy, Debug, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SolanaCluster {
    #[default]
    MainnetBeta,
    Testnet,
    Devnet,
    Custom,
}

/// How the validator catches up. Replaying every slot from genesis takes
/// longer than the chain takes to produce them, so a new validator fetches a
/// snapshot instead.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SolanaSnapshotSpec {
    #[serde(default)]
    pub fetch: bool,
    #[serde(default)]
    pub interval_slots: i64,
    #[serde(default)]
    pub minimum_download_speed_mbps: i32,
}

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "bootno.de",
    version = "v1",
    kind = "SolanaRuntime",
    plural = "solanaruntimes",
    namespaced,
    status = "SolanaRuntimeStatus",
    shortname = "solrt"
)]
#[serde(rename_all = "camelCase")]
pub struct SolanaRuntimeSpec {
    pub node_image: ImageSpec,
    pub cluster: SolanaCluster,
    /// The validator's own identity.
    pub identity_keypair: SecretRef,
    /// Absent means the node follows the cluster without voting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote_account: Option<SecretRef>,
    #[serde(default)]
    pub rpc_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<SolanaSnapshotSpec>,
    /// PVC template for the ledger.
    pub ledger: StorageSpec,
    #[serde(default)]
    pub entry_points: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceRequirements>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    #[serde(default)]
    pub rpc_port: i32,
    #[serde(default)]
    pub gossip_port: i32,
    #[serde(default)]
    pub image_pull_secrets: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SolanaRuntimeStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default)]
    pub ready: bool,
    #[serde(default)]
    pub slot_height: i64,
    #[serde(default)]
    pub root_slot: i64,
    #[serde(default)]
    pub vote_slot: i64,
    /// Following the cluster is not the same as voting on it.
    #[serde(default)]
    pub is_voting: bool,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default)]
    pub observed_generation: i64,
}
