//! Tenant reconciler — per-tenant one-click-deploy authorization.
//!
//! When the platform (cloud) onboards an org it creates the tenant
//! namespace `tenant-<org>` labeled `hanzo.ai/managed-by=platform`. Cloud's
//! deploy path BLOCKS (`waitForTenantRBAC` — a SelfSubjectAccessReview poll)
//! until this controller has projected, into every such namespace:
//!
//!   1. a namespaced [`RoleBinding`] `cloud` → ClusterRole
//!      `hanzo-cloud-platform-tenant`, bound to the `hanzo/cloud`
//!      ServiceAccount, so cloud may act (resourcequotas / limitranges /
//!      services.hanzo.ai …) inside that ONE tenant — never cluster-wide, and
//!   2. a `ghcr-pull` `kubernetes.io/dockerconfigjson` image-pull [`Secret`] so
//!      tenant pods can pull the PRIVATE per-tenant build image
//!      (`ghcr.io/hanzoai/tenant-<org>/…`). cloud holds NO `secrets` grant;
//!      the operator is the designated K8s-secret handler (KMS-only secrets
//!      model), projecting the payload from the KMS-synced source Secret.
//!
//! Both projected children are owner-referenced to the Namespace and re-stamped
//! with `hanzo.ai/managed-by=platform`, so they are identifiable as
//! platform-managed and GC with the tenant.
//!
//! ## CRITICAL — RoleBinding, never ClusterRoleBinding
//!
//! A ClusterRoleBinding would grant cloud deploy rights in EVERY namespace:
//! a cross-tenant deploy hole where onboarding org A could deploy into org B's
//! namespace. The NAMESPACED RoleBinding confines the grant to the single
//! tenant namespace it lives in, so the blast radius of the cloud SA is
//! exactly the set of tenants that have been onboarded — and each grant is
//! independently revocable by deleting one RoleBinding.
//!
//! ## Gate
//!
//! Master enable `TENANT_CONTROLLER` (default `true`; set `false` to
//! disable). Unlike the fleet-mutating apps/kms controllers this one only
//! ADDS narrowly-scoped objects to namespaces already marked as platform-managed
//! tenants, so it is safe to run by default and IS the one-click-deploy mechanism.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Namespace;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::api::rbac::v1::{RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::ByteString;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config as WatcherConfig;
use kube::{Client, Resource, ResourceExt};
use std::collections::BTreeMap;
use tracing::{error, info, warn};

use super::owner_ref_for;
use crate::apply::apply;
use crate::core::secret::{is_operator_managed, validate_secret_value, MANAGED_BY_LABEL};
use crate::core::{OperatorError, Result};

/// The image-pull Secret the operator projects into each tenant namespace. The
/// platform Service CR references it BY NAME (imagePullSecrets); cloud never
/// creates it. `managed-by` value distinct from every other operator secret path
/// so the strict hijack guard only ever adopts THIS controller's Secrets.
const PULL_SECRET_MANAGER: &str = "hanzo-operator-tenant";

/// The one namespaced RoleBinding name the operator manages per tenant.
///
/// Named for the identity it grants, which is `cloud`. It was
/// `cloud-api-platform`: `-api` for a ServiceAccount that outlived the name (the
/// workload, its Service and its App CR are all `cloud`), and `-platform` for the
/// ClusterRole it points at, which roleRef already states.
const BINDING_NAME: &str = "cloud";

/// The platform-managed marker. Cloud stamps it on the tenant namespace; the
/// operator re-stamps it on every projected child so both are identifiable as
/// platform-managed (matches cloud's `hanzo.ai/managed-by=platform`).
const PLATFORM_MANAGED_KEY: &str = "hanzo.ai/managed-by";
const PLATFORM_MANAGED_VALUE: &str = "platform";

/// Resolved tenant configuration. Hanzo defaults; env overrides let the
/// same operator binary serve a white-label universe whose platform SA lives in
/// a different namespace or whose tenant ClusterRole is named differently.
#[derive(Clone, Debug)]
pub struct Config {
    /// `key=value` label that marks a namespace as a platform-managed tenant.
    pub tenant_label: String,
    /// Namespace of the ServiceAccount that deploys into tenants (cloud's
    /// home namespace).
    pub sa_namespace: String,
    /// Name of that ServiceAccount.
    pub sa_name: String,
    /// ClusterRole the RoleBinding grants (scoped to the tenant namespace).
    pub cluster_role: String,
    /// Name of the image-pull Secret projected into each tenant namespace. The
    /// platform Service CR references this by name so pods can pull the PRIVATE
    /// per-tenant build image (ghcr.io/<org>/tenant-<org>/*).
    pub pull_secret_name: String,
    /// Namespace of the KMS-synced SOURCE dockerconfigjson the per-tenant pull
    /// Secret is projected from (a Secret a KMSSecret CR syncs from Hanzo KMS).
    pub pull_source_namespace: String,
    /// Name of that source Secret.
    pub pull_source_name: String,
    /// Key within both the source and the projected Secret holding the docker
    /// config JSON (standard `.dockerconfigjson`).
    pub pull_config_key: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tenant_label: "hanzo.ai/managed-by=platform".to_string(),
            sa_namespace: "hanzo".to_string(),
            // The identity cloud runs as. `cloud-api` was the same identity under
            // an older name; the ServiceAccount is the last object still carrying
            // it. TENANT_SA_NAME overrides it for a white-label universe.
            sa_name: "cloud".to_string(),
            cluster_role: "hanzo-cloud-platform-tenant".to_string(),
            pull_secret_name: "ghcr-pull".to_string(),
            pull_source_namespace: "hanzo".to_string(),
            pull_source_name: "ghcr-secret".to_string(),
            pull_config_key: ".dockerconfigjson".to_string(),
        }
    }
}

impl Config {
    /// Read overrides from the environment, falling back to the Hanzo defaults.
    pub fn from_env() -> Self {
        let d = Config::default();
        Config {
            tenant_label: std::env::var("TENANT_LABEL").unwrap_or(d.tenant_label),
            sa_namespace: std::env::var("TENANT_SA_NAMESPACE").unwrap_or(d.sa_namespace),
            sa_name: std::env::var("TENANT_SA_NAME").unwrap_or(d.sa_name),
            cluster_role: std::env::var("TENANT_CLUSTER_ROLE").unwrap_or(d.cluster_role),
            pull_secret_name: std::env::var("TENANT_PULL_SECRET_NAME")
                .unwrap_or(d.pull_secret_name),
            pull_source_namespace: std::env::var("TENANT_PULL_SOURCE_NAMESPACE")
                .unwrap_or(d.pull_source_namespace),
            pull_source_name: std::env::var("TENANT_PULL_SOURCE_NAME")
                .unwrap_or(d.pull_source_name),
            pull_config_key: std::env::var("TENANT_PULL_CONFIG_KEY").unwrap_or(d.pull_config_key),
        }
    }

    /// The label KEY (left of `=`) — what the reconcile guard checks presence of.
    fn label_key(&self) -> &str {
        self.tenant_label
            .split('=')
            .next()
            .unwrap_or(&self.tenant_label)
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub cfg: Config,
}

/// Extract the org slug for a tenant namespace: prefer the explicit
/// `hanzo.ai/org` label the platform stamps, else strip the `tenant-` prefix.
fn org_of(ns: &Namespace) -> String {
    if let Some(org) = ns.labels().get("hanzo.ai/org") {
        if !org.is_empty() {
            return org.clone();
        }
    }
    ns.name_any()
        .strip_prefix("tenant-")
        .unwrap_or(&ns.name_any())
        .to_string()
}

/// Shared metadata for a projected child: named in the tenant namespace, stamped
/// with the platform-managed + org labels, owner-referenced to the Namespace so
/// it GCs with the tenant. `extra` carries the child-specific label (e.g. the
/// Secret's hijack-guard `managed-by`).
fn managed_meta(
    name: &str,
    ns: &str,
    org: &str,
    owner: OwnerReference,
    extra: &[(&str, &str)],
) -> ObjectMeta {
    let mut labels = BTreeMap::new();
    labels.insert(
        PLATFORM_MANAGED_KEY.to_string(),
        PLATFORM_MANAGED_VALUE.to_string(),
    );
    labels.insert("hanzo.ai/org".to_string(), org.to_string());
    for (k, v) in extra {
        labels.insert(k.to_string(), v.to_string());
    }
    ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(ns.to_string()),
        labels: Some(labels),
        owner_references: Some(vec![owner]),
        ..Default::default()
    }
}

/// Build the desired namespaced RoleBinding for a tenant namespace. Pure — the
/// unit of test. Always a `RoleBinding` (namespaced), never a ClusterRoleBinding.
pub fn build_rolebinding(
    ns_name: &str,
    org: &str,
    cfg: &Config,
    owner: OwnerReference,
) -> RoleBinding {
    RoleBinding {
        metadata: managed_meta(BINDING_NAME, ns_name, org, owner, &[]),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: cfg.cluster_role.clone(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: cfg.sa_name.clone(),
            namespace: Some(cfg.sa_namespace.clone()),
            // ServiceAccount subjects carry the core ("") API group; omitting it
            // defaults to "" per the RBAC contract.
            api_group: None,
        }]),
    }
}

/// Build the desired per-tenant image-pull Secret. Pure — the unit of test.
/// Always a `kubernetes.io/dockerconfigjson` Secret carrying the operator's
/// `managed-by` label (so the hijack guard adopts only our own), the platform +
/// org labels, and the Namespace owner reference; `dockerconfig` is the raw JSON
/// bytes projected from the KMS-synced source (k8s-openapi base64-encodes on the
/// wire).
pub fn build_pull_secret(
    ns: &str,
    org: &str,
    cfg: &Config,
    dockerconfig: Vec<u8>,
    owner: OwnerReference,
) -> Secret {
    let mut data = BTreeMap::new();
    data.insert(cfg.pull_config_key.clone(), ByteString(dockerconfig));

    Secret {
        metadata: managed_meta(
            &cfg.pull_secret_name,
            ns,
            org,
            owner,
            &[(MANAGED_BY_LABEL, PULL_SECRET_MANAGER)],
        ),
        type_: Some("kubernetes.io/dockerconfigjson".to_string()),
        data: Some(data),
        ..Default::default()
    }
}

/// Extract the docker-config bytes from a source Secret under `key`, checking
/// `data` (base64-decoded by k8s-openapi) then `string_data`. Pure.
fn pull_config_bytes(src: &Secret, key: &str) -> Option<Vec<u8>> {
    if let Some(d) = src.data.as_ref().and_then(|m| m.get(key)) {
        return Some(d.0.clone());
    }
    src.string_data
        .as_ref()
        .and_then(|m| m.get(key))
        .map(|s| s.clone().into_bytes())
}

/// Ensure the per-tenant image-pull Secret exists in `ns`, projected from the
/// KMS-synced source. The OPERATOR is the designated K8s-secret handler here —
/// cloud holds no `secrets` grant and creates no Secret. Fail-OPEN on a
/// missing/empty source (log + skip) so a source outage never blocks the deploy
/// RoleBinding; hijack-guarded so an unmanaged Secret of the same name is never
/// overwritten; SSA-applied like the RoleBinding.
async fn ensure_pull_secret(ns: &str, org: &str, ctx: &Ctx, owner: OwnerReference) -> Result<()> {
    let src_api: Api<Secret> = Api::namespaced(ctx.client.clone(), &ctx.cfg.pull_source_namespace);
    let source = match src_api.get_opt(&ctx.cfg.pull_source_name).await? {
        Some(s) => s,
        None => {
            warn!(
                namespace = %ns,
                source = %format!("{}/{}", ctx.cfg.pull_source_namespace, ctx.cfg.pull_source_name),
                "tenant pull-secret source not found — skipping (deploy RoleBinding still applied)"
            );
            return Ok(());
        }
    };
    let bytes = match pull_config_bytes(&source, &ctx.cfg.pull_config_key) {
        Some(b) if !b.is_empty() => b,
        _ => {
            warn!(
                namespace = %ns, key = %ctx.cfg.pull_config_key,
                "tenant pull-secret source missing/empty docker-config key — skipping"
            );
            return Ok(());
        }
    };
    validate_secret_value(&bytes)
        .map_err(|e| OperatorError::Config(format!("pull-secret source value: {e}")))?;

    // Strict hijack guard: never overwrite a same-named Secret we do not own.
    let dst_api: Api<Secret> = Api::namespaced(ctx.client.clone(), ns);
    if let Some(existing) = dst_api.get_opt(&ctx.cfg.pull_secret_name).await? {
        if !is_operator_managed(&existing, PULL_SECRET_MANAGER, "") {
            return Err(OperatorError::Config(format!(
                "refuse to overwrite unmanaged Secret {ns}/{}",
                ctx.cfg.pull_secret_name
            )));
        }
    }

    let secret = build_pull_secret(ns, org, &ctx.cfg, bytes, owner);
    apply(&dst_api, &secret).await?;
    info!(
        namespace = %ns, org = %org, secret = %ctx.cfg.pull_secret_name,
        "ensured tenant image-pull Secret (KMS-synced source)"
    );
    Ok(())
}

pub async fn reconcile(ns: Arc<Namespace>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns_name = ns.name_any();

    // Defense in depth: the watcher already label-filters, but never bind a
    // namespace that isn't a platform-managed tenant. `roleRef` is immutable, so
    // a mistaken bind would need manual deletion — refuse up front instead.
    let is_tenant = ns.labels().contains_key(ctx.cfg.label_key());
    if !is_tenant {
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    // A namespace being deleted must not have work re-applied against it.
    if ns.meta().deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }

    let org = org_of(&ns);
    // Both children are owned by the Namespace so they GC with the tenant.
    let owner = owner_ref_for(ns.as_ref(), "v1", "Namespace");

    let rb = build_rolebinding(&ns_name, &org, &ctx.cfg, owner.clone());
    let api: Api<RoleBinding> = Api::namespaced(ctx.client.clone(), &ns_name);
    apply(&api, &rb).await?;
    info!(
        namespace = %ns_name, org = %org, binding = BINDING_NAME,
        cluster_role = %ctx.cfg.cluster_role,
        subject = %format!("{}/{}", ctx.cfg.sa_namespace, ctx.cfg.sa_name),
        "ensured tenant deploy RoleBinding (namespace-scoped)"
    );

    // Also project the per-tenant image-pull Secret from the KMS-synced source so
    // pods can pull the PRIVATE per-tenant build image. Fail-open: a provisioning
    // error (source outage, hijack-guard refusal, apiserver hiccup) is logged and
    // retried next tick — the deploy RoleBinding above already applied, so deploy
    // AUTHZ is never blocked by pull-secret trouble.
    if let Err(e) = ensure_pull_secret(&ns_name, &org, &ctx, owner).await {
        warn!(namespace = %ns_name, error = %e, "tenant image-pull Secret provisioning failed (RoleBinding applied; retry next tick)");
    }

    Ok(Action::requeue(Duration::from_secs(300)))
}

pub fn on_error(_obj: Arc<Namespace>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "tenant reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

/// Watch platform-managed tenant namespaces and ensure the per-tenant deploy
/// RoleBinding + image-pull Secret. `enabled` is the master gate
/// (`TENANT_CONTROLLER`).
pub async fn run_tenant_controller(client: Client, enabled: bool) {
    if !enabled {
        info!("Tenant controller disabled (TENANT_CONTROLLER=false)");
        return;
    }
    let cfg = Config::from_env();
    info!(
        tenant_label = %cfg.tenant_label,
        subject = %format!("{}/{}", cfg.sa_namespace, cfg.sa_name),
        cluster_role = %cfg.cluster_role,
        "Starting Tenant controller (namespace-scoped RoleBindings)"
    );

    // Tenant RoleBindings are cluster-wide by their namespaces, so watch all
    // namespaces — but only those carrying the tenant marker label. The
    // controller never touches a namespace the platform didn't mark.
    let api: Api<Namespace> = Api::all(client.clone());
    let watch = WatcherConfig::default().labels(&cfg.tenant_label);
    if watch.label_selector.is_none() {
        warn!("empty tenant label selector — refusing to watch ALL namespaces");
        return;
    }

    let ctx = Arc::new(Ctx { client, cfg });
    Controller::new(api, watch)
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns_with(name: &str, labels: &[(&str, &str)]) -> Namespace {
        let mut m = BTreeMap::new();
        for (k, v) in labels {
            m.insert(k.to_string(), v.to_string());
        }
        Namespace {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(m),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "v1".to_string(),
            kind: "Namespace".to_string(),
            name: "tenant-acme".to_string(),
            uid: "uid-1".to_string(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    #[test]
    fn binding_is_namespaced_and_confined_to_the_tenant() {
        let cfg = Config::default();
        let rb = build_rolebinding("tenant-acme", "acme", &cfg, owner());
        // NAMESPACED: the binding lives in the tenant namespace — this is the
        // single control that keeps cloud out of every other tenant.
        assert_eq!(rb.metadata.namespace.as_deref(), Some("tenant-acme"));
        assert_eq!(rb.metadata.name.as_deref(), Some(BINDING_NAME));
    }

    #[test]
    fn binding_targets_the_tenant_clusterrole_and_cloud_sa() {
        let cfg = Config::default();
        let rb = build_rolebinding("tenant-acme", "acme", &cfg, owner());
        assert_eq!(rb.role_ref.kind, "ClusterRole");
        assert_eq!(rb.role_ref.name, "hanzo-cloud-platform-tenant");
        assert_eq!(rb.role_ref.api_group, "rbac.authorization.k8s.io");
        let s = rb.subjects.unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, "ServiceAccount");
        assert_eq!(s[0].name, "cloud");
        assert_eq!(s[0].namespace.as_deref(), Some("hanzo"));
    }

    #[test]
    fn binding_is_platform_labeled_and_owned_by_the_namespace() {
        let cfg = Config::default();
        let rb = build_rolebinding("tenant-acme", "acme", &cfg, owner());
        // GC + identification: platform-managed label + Namespace owner reference.
        let labels = rb.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels.get("hanzo.ai/managed-by").map(String::as_str),
            Some("platform")
        );
        assert_eq!(labels.get("hanzo.ai/org").map(String::as_str), Some("acme"));
        let owners = rb.metadata.owner_references.as_ref().unwrap();
        assert_eq!(owners.len(), 1);
        assert_eq!(owners[0].kind, "Namespace");
        assert_eq!(owners[0].name, "tenant-acme");
    }

    #[test]
    fn org_prefers_label_then_falls_back_to_name_prefix() {
        // Explicit label wins.
        let ns = ns_with("tenant-maxpower", &[("hanzo.ai/org", "maxpower")]);
        assert_eq!(org_of(&ns), "maxpower");
        // Fallback: strip the tenant- prefix.
        let ns = ns_with("tenant-hanzo", &[]);
        assert_eq!(org_of(&ns), "hanzo");
    }

    #[test]
    fn env_overrides_apply_for_white_label() {
        let cfg = Config {
            tenant_label: "lux.cloud/managed-by=platform".to_string(),
            sa_namespace: "lux".to_string(),
            sa_name: "lux-cloud".to_string(),
            cluster_role: "lux-cloud-platform-tenant".to_string(),
            ..Config::default()
        };
        assert_eq!(cfg.label_key(), "lux.cloud/managed-by");
        let rb = build_rolebinding("tenant-foo", "foo", &cfg, owner());
        assert_eq!(rb.role_ref.name, "lux-cloud-platform-tenant");
        assert_eq!(rb.subjects.unwrap()[0].namespace.as_deref(), Some("lux"));
    }

    #[test]
    fn label_key_splits_on_equals() {
        let cfg = Config::default();
        assert_eq!(cfg.label_key(), "hanzo.ai/managed-by");
    }

    #[test]
    fn pull_secret_defaults_are_ghcr_pull_from_hanzo_source() {
        let d = Config::default();
        assert_eq!(d.pull_secret_name, "ghcr-pull");
        assert_eq!(d.pull_source_namespace, "hanzo");
        assert_eq!(d.pull_source_name, "ghcr-secret");
        assert_eq!(d.pull_config_key, ".dockerconfigjson");
    }

    #[test]
    fn pull_secret_is_dockerconfigjson_typed_and_confined_to_tenant() {
        let cfg = Config::default();
        let s = build_pull_secret(
            "tenant-acme",
            "acme",
            &cfg,
            b"{\"auths\":{}}".to_vec(),
            owner(),
        );
        // Correct Secret TYPE — an Opaque secret is ignored as an imagePullSecret.
        assert_eq!(s.type_.as_deref(), Some("kubernetes.io/dockerconfigjson"));
        // Lives in the tenant namespace under the CR-referenced name.
        assert_eq!(s.metadata.namespace.as_deref(), Some("tenant-acme"));
        assert_eq!(s.metadata.name.as_deref(), Some("ghcr-pull"));
        // Carries the docker-config under the standard key with the source bytes.
        let data = s.data.as_ref().unwrap();
        assert_eq!(data.get(".dockerconfigjson").unwrap().0, b"{\"auths\":{}}");
        // Platform + org labels for identification; Namespace owner for GC.
        let labels = s.metadata.labels.as_ref().unwrap();
        assert_eq!(
            labels.get("hanzo.ai/managed-by").map(String::as_str),
            Some("platform")
        );
        assert_eq!(labels.get("hanzo.ai/org").map(String::as_str), Some("acme"));
        assert_eq!(
            s.metadata.owner_references.as_ref().unwrap()[0].kind,
            "Namespace"
        );
    }

    #[test]
    fn pull_secret_managed_by_a_distinct_manager_no_cross_adoption() {
        // The projected pull Secret is adopted by the pull-secret manager, and
        // NOT by the RoleBinding/KMS-zap managers — the strict hijack guard keeps
        // the operator secret paths from ever overwriting each other.
        let cfg = Config::default();
        let s = build_pull_secret("tenant-acme", "acme", &cfg, b"x".to_vec(), owner());
        assert!(is_operator_managed(&s, PULL_SECRET_MANAGER, ""));
        assert!(!is_operator_managed(&s, "hanzo-operator", ""));
        assert!(!is_operator_managed(&s, "hanzo-operator-kms", ""));
    }

    #[test]
    fn pull_config_bytes_reads_data_then_string_data() {
        // data (base64-decoded by k8s-openapi into ByteString) is preferred.
        let mut data = BTreeMap::new();
        data.insert(
            ".dockerconfigjson".to_string(),
            ByteString(b"from-data".to_vec()),
        );
        let s = Secret {
            data: Some(data),
            ..Default::default()
        };
        assert_eq!(
            pull_config_bytes(&s, ".dockerconfigjson"),
            Some(b"from-data".to_vec())
        );
        // string_data fallback.
        let mut sd = BTreeMap::new();
        sd.insert(".dockerconfigjson".to_string(), "from-string".to_string());
        let s2 = Secret {
            string_data: Some(sd),
            ..Default::default()
        };
        assert_eq!(
            pull_config_bytes(&s2, ".dockerconfigjson"),
            Some(b"from-string".to_vec())
        );
        // Missing key → None (caller fails open).
        assert_eq!(
            pull_config_bytes(&Secret::default(), ".dockerconfigjson"),
            None
        );
    }

    #[test]
    fn pull_secret_honors_white_label_config() {
        // A white-label universe points at its own source + secret names.
        let cfg = Config {
            pull_secret_name: "lux-pull".to_string(),
            pull_source_namespace: "lux".to_string(),
            pull_source_name: "lux-ghcr".to_string(),
            pull_config_key: ".dockerconfigjson".to_string(),
            ..Config::default()
        };
        let s = build_pull_secret("tenant-foo", "foo", &cfg, b"y".to_vec(), owner());
        assert_eq!(s.metadata.name.as_deref(), Some("lux-pull"));
    }
}
