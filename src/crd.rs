//! Custom Resource Definitions for the Hanzo operator.
//!
//! All 24 Kinds at `hanzo.ai/v1` (the compile-time default). For other
//! universes (lux.cloud, zoo.cloud, osage.cloud), generate CRD YAMLs with
//! the `generate-crd-yaml` binary, which rewrites the group at install time.
//!
//! ## v0.3.0 schema
//!
//! - Legacy `HanzoService`/`HanzoDatastore`/`HanzoDNS` removed (one way only).
//! - `BaseApp` renamed to `Base`.
//! - 4 new Kinds: `SPA`, `Queue`, `Observability`, `Function`.
//! - Run `scripts/migrate-v0.2-to-v0.3.sh` before rolling v0.3.0 to cluster.
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

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProbeSpec {
    #[serde(default)]
    pub path: String,
    pub port: i32,
    #[serde(default)]
    pub initial_delay_seconds: i32,
    #[serde(default)]
    pub period_seconds: i32,
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

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Datastore",
    plural = "datastores",
    namespaced,
    status = "DatastoreStatus",
    shortname = "hds",
    printcolumn = r#"{"name":"Type","type":"string","jsonPath":".spec.type"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct DatastoreSpec {
    #[serde(rename = "type")]
    pub type_: String,
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
}

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
    pub subnet_id: String,
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
#[serde(rename_all = "camelCase")]
pub struct NetworkSpec {
    #[serde(rename = "networkID")]
    pub network_id: String,
    pub validators: ValidatorSpec,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainSpec>,
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

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct NetworkStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
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
// Base Kind (renamed from BaseApp in v0.3.0)
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
    namespaced,
    status = "BaseStatus",
    shortname = "base"
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
pub struct SQLSpec(pub DatastoreSpec);

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
pub struct KVSpec(pub DatastoreSpec);

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
#[serde(rename_all = "camelCase")]
pub struct DocDBSpec(pub DatastoreSpec);

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
pub struct S3Spec(pub DatastoreSpec);

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

#[derive(CustomResource, Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "hanzo.ai",
    version = "v1",
    kind = "Subnet",
    plural = "subnets",
    namespaced,
    status = "NetworkStatus",
    shortname = "subnet"
)]
#[serde(rename_all = "camelCase")]
pub struct SubnetSpec {
    pub network: String,
    pub subnet_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<ChainSpec>,
}

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
