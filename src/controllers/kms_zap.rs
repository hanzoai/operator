//! Additive ZAP-native KMSSecret reconciler.
//!
//! Projects KMS secrets into k8s Secrets over the ZAP binary protocol
//! (`crate::zapclient`) for `kms.<universe>/v1 KMSSecret` CRs explicitly
//! marked ZAP-native (`spec.transport == "zap"`). Purely ADDITIVE and OPT-IN
//! (`KMS_ZAP_CONTROLLER=true`): CRs without that marker are IGNORED, so the
//! legacy REST projector remains the only secret path until cutover.
//!
//! One CRD family — the same `kms.<universe>/v1 KMSSecret` the operator
//! already writes (`controllers::service::reconcile_kms_secret`), watched
//! group-erased as a DynamicObject so it works under every universe api-group.
//!
//! Fail-closed: any ZAP fetch error → no Secret write (never a plaintext
//! default). Reuses the canonical guards in `core::secret` (strict
//! hijack-protection + control-byte rejection).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::Api;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::Client;
use serde::Deserialize;
use tracing::{info, warn};

use crate::apply;
use crate::core::secret::{
    is_operator_managed, owner_ref, validate_secret_value, MANAGED_BY_LABEL,
};
use crate::zapclient::ZapClient;

/// managed-by value for Secrets this controller owns — distinct from the REST
/// projector's so the two never adopt each other's Secrets.
pub const KMS_ZAP_MANAGER: &str = "hanzo-operator-kms-zap";

/// The KMS CRD family rides the universe's own API group, prefixed: a hanzo
/// universe serves `kms.hanzo.ai`, a lux one `kms.lux.cloud`. It was fixed at
/// `kms.hanzo.ai` on the reasoning that KMS is one service everywhere, but the
/// cluster grants `kms.lux.cloud` — a fixed group is only correct while every
/// universe answers to the same name, and they do not. Derive it and both hold.
///
/// There is ONE version. `v1alpha1` promised a compatibility story nobody was
/// keeping; `api_group::API_VERSION` is where the answer lives for every other
/// CRD, and this is not the exception it was written as.
use crate::api_group::API_VERSION as KMS_VERSION;
const KMS_KIND: &str = "KMSSecret";

// Input bounds (mirror the proven KMS-bridge limits).
const MAX_ADDR: usize = 256;
const MAX_FIELD: usize = 256;
const MAX_KEY: usize = 128;
const MAX_KEYS: usize = 128;
const MAX_NAME: usize = 253;

/// The ZAP-native subset of a `KMSSecret` spec. Only CRs whose spec carries
/// `transport: "zap"` are handled here.
#[derive(Debug, Clone, Deserialize, Default)]
struct ZapKmsSpec {
    #[serde(default)]
    transport: String,
    #[serde(default, rename = "zapAddr")]
    zap_addr: String,
    #[serde(default, rename = "projectSlug")]
    project_slug: String,
    #[serde(default, rename = "envSlug")]
    env_slug: String,
    #[serde(default, rename = "secretsPath")]
    secrets_path: String,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default, rename = "managedSecretName")]
    managed_secret_name: String,
    #[serde(default, rename = "allowedNamespaces")]
    allowed_namespaces: Vec<String>,
    #[serde(default, rename = "clusterName")]
    cluster_name: String,
    #[serde(default, rename = "secretType")]
    secret_type: String,
    #[serde(default, rename = "creationPolicy")]
    creation_policy: String,
    #[serde(default, rename = "secretNamespace")]
    secret_namespace: String,
    #[serde(default)]
    rename: BTreeMap<String, String>,
    #[serde(default)]
    literals: BTreeMap<String, String>,
    #[serde(default, rename = "resyncInterval")]
    resync_interval: i64,
}

/// How often to refetch. Zero means the projector's own cadence; anything under
/// a floor would be a tight loop against KMS rather than a refresh.
fn resync(s: &ZapKmsSpec) -> Duration {
    const DEFAULT: u64 = 300;
    const FLOOR: u64 = 30;
    if s.resync_interval <= 0 {
        return Duration::from_secs(DEFAULT);
    }
    Duration::from_secs((s.resync_interval as u64).max(FLOOR))
}

/// The Secret's type. Empty means Opaque, which is also what the kubelet
/// assumes — but a pull secret it type-checks and SKIPS without logging, so the
/// declared value has to reach the object.
fn secret_type(s: &ZapKmsSpec) -> Option<String> {
    if s.secret_type.is_empty() || s.secret_type == "Opaque" {
        None
    } else {
        Some(s.secret_type.clone())
    }
}

/// Whether the projected Secret is collected with the CR.
///
/// Orphan is the default: deleting a reference must not pull env out from under
/// a running pod. An owner reference also cannot cross a namespace, so a
/// cross-namespace projection is Orphan whatever it asked for — the alternative
/// is a Secret the garbage collector removes the moment it appears.
fn owned(s: &ZapKmsSpec, cr_ns: &str, target_ns: &str) -> bool {
    s.creation_policy == "Owner" && cr_ns == target_ns
}

/// Where the Secret is written: the CR's own namespace unless it names another.
fn target_namespace(s: &ZapKmsSpec, cr_ns: &str) -> String {
    if s.secret_namespace.is_empty() {
        cr_ns.to_string()
    } else {
        s.secret_namespace.clone()
    }
}

/// Apply the rename map: Secret key <- KMS key. A rename naming a key that was
/// not fetched is refused rather than dropped, because a Secret missing a key
/// fails at the next pod creation, not here where the reason is legible.
fn apply_rename(
    s: &ZapKmsSpec,
    fetched: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    if s.rename.is_empty() {
        return Ok(fetched);
    }
    let mut out = BTreeMap::new();
    for (to, from) in &s.rename {
        let v = fetched
            .get(from)
            .ok_or_else(|| format!("rename {to} <- {from}: {from} is not among the fetched keys"))?;
        out.insert(to.clone(), v.clone());
    }
    // Anything not renamed keeps its own name.
    for (k, v) in fetched {
        if !s.rename.values().any(|from| *from == k) {
            out.insert(k, v);
        }
    }
    Ok(out)
}

/// True iff this CR opts into the ZAP-native path.
fn is_zap_native(spec: &ZapKmsSpec) -> bool {
    spec.transport == "zap"
}

/// Validate a ZAP-native spec before any I/O. Pure.
fn validate_spec(s: &ZapKmsSpec) -> Result<(), String> {
    if !is_zap_native(s) {
        return Err("not zap-native".into());
    }
    if s.zap_addr.is_empty() || s.zap_addr.len() > MAX_ADDR {
        return Err("zapAddr empty or too long".into());
    }
    if s.project_slug.is_empty() || s.project_slug.len() > MAX_FIELD {
        return Err("projectSlug empty or too long".into());
    }
    if s.env_slug.is_empty() || s.env_slug.len() > MAX_FIELD {
        return Err("envSlug empty or too long".into());
    }
    if s.secrets_path.len() > MAX_FIELD {
        return Err("secretsPath too long".into());
    }
    if s.managed_secret_name.is_empty() || s.managed_secret_name.len() > MAX_NAME {
        return Err("managedSecretName empty or too long".into());
    }
    if s.keys.is_empty() || s.keys.len() > MAX_KEYS {
        return Err("keys empty or too many".into());
    }
    for k in &s.keys {
        if k.is_empty() || k.len() > MAX_KEY {
            return Err("key empty or too long".into());
        }
    }
    Ok(())
}

/// Namespace allow-list: target must be the CR's own namespace, or explicitly
/// allow-listed. No cross-namespace by default.
fn namespace_allowed(s: &ZapKmsSpec, cr_ns: &str, target_ns: &str) -> bool {
    target_ns == cr_ns || s.allowed_namespaces.iter().any(|n| n == target_ns)
}

struct Ctx {
    client: Client,
    /// Resolved `kms.<universe>` group, for the ownerRef on projected Secrets.
    group: String,
}

#[derive(Debug, thiserror::Error)]
enum ReconcileError {
    #[error("{0}")]
    Msg(String),
}
impl From<String> for ReconcileError {
    fn from(s: String) -> Self {
        ReconcileError::Msg(s)
    }
}

async fn reconcile(obj: Arc<DynamicObject>, ctx: Arc<Ctx>) -> Result<Action, ReconcileError> {
    let name = obj.metadata.name.clone().unwrap_or_default();
    let cr_ns = obj.metadata.namespace.clone().unwrap_or_default();

    // Decode the spec; IGNORE non-zap-native CRs (left to the REST projector).
    let spec: ZapKmsSpec = obj
        .data
        .get("spec")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("spec decode: {e}"))?
        .unwrap_or_default();
    if !is_zap_native(&spec) {
        return Ok(Action::await_change());
    }
    validate_spec(&spec)?;

    // The managed Secret lives in the CR's namespace.
    let target_ns = target_namespace(&spec, &cr_ns);
    if !namespace_allowed(&spec, &cr_ns, &target_ns) {
        return Err(format!("namespace {target_ns} not allowed").into());
    }

    // Fetch every key over ZAP — FAIL-CLOSED: any error aborts with no write.
    let mut zap = ZapClient::connect(&spec.zap_addr, &spec.cluster_name)
        .await
        .map_err(|e| format!("zap connect {}: {e}", spec.zap_addr))?;
    let mut data: BTreeMap<String, String> = BTreeMap::new();
    for key in &spec.keys {
        let val = zap
            .get_secret(&spec.secrets_path, key, &spec.env_slug)
            .await
            .map_err(|e| format!("zap get {key}: {e}"))?;
        validate_secret_value(val.as_bytes())?;
        data.insert(key.clone(), val);
    }

    // Strict hijack guard: never overwrite a Secret we don't own.
    let mut data = apply_rename(&spec, data)?;
    for (k, v) in &spec.literals {
        if data.contains_key(k) {
            return Err(format!(
                "literal {k} collides with a fetched key; one of them would win silently"
            )
            .into());
        }
        data.insert(k.clone(), v.clone());
    }

    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &target_ns);
    let cr_uid = obj.metadata.uid.clone().unwrap_or_default();
    if let Some(existing) = secrets
        .get_opt(&spec.managed_secret_name)
        .await
        .map_err(|e| format!("get secret: {e}"))?
    {
        if !is_operator_managed(&existing, KMS_ZAP_MANAGER, &cr_uid) {
            return Err(format!(
                "refuse to overwrite unmanaged Secret {target_ns}/{}",
                spec.managed_secret_name
            )
            .into());
        }
    }

    // Project via SSA. managed-by label + ownerRef so only we adopt it.
    let mut labels = BTreeMap::new();
    labels.insert(MANAGED_BY_LABEL.to_string(), KMS_ZAP_MANAGER.to_string());
    let owner = owner_ref(
        &format!("{}/{KMS_VERSION}", ctx.group),
        KMS_KIND,
        &name,
        &cr_uid,
    );
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some(spec.managed_secret_name.clone()),
            namespace: Some(target_ns.clone()),
            labels: Some(labels),
            owner_references: if owned(&spec, &cr_ns, &target_ns) {
                Some(vec![owner])
            } else {
                None
            },
            ..Default::default()
        },
        type_: secret_type(&spec),
        string_data: Some(data),
        ..Default::default()
    };
    apply::apply(&secrets, &secret)
        .await
        .map_err(|e| format!("apply secret: {e}"))?;

    info!(
        name,
        namespace = target_ns,
        keys = spec.keys.len(),
        "KMSSecret (zap) reconciled"
    );
    Ok(Action::requeue(resync(&spec)))
}

fn on_error(_obj: Arc<DynamicObject>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    warn!(error = %err, "KMS ZAP reconcile error");
    Action::requeue(Duration::from_secs(30))
}

/// Opt-in entrypoint. When `enabled` is false this returns immediately — the
/// controller never watches anything, so the REST projector stays the only
/// secret path until a deliberate cutover. Matches the call shape of the
/// other controllers so it slots into `run_all_controllers`' `join!`.
pub async fn run_kms_zap_controller(
    client: Client,
    namespace: String,
    enabled: bool,
    api_group: &str,
) {
    if !enabled {
        info!("KMS ZAP controller disabled (set KMS_ZAP_CONTROLLER=true to enable)");
        return;
    }
    let group = crate::install::family_of(KMS_KIND).group(api_group);
    let gvk = GroupVersionKind::gvk(&group, KMS_VERSION, KMS_KIND);
    let ar = ApiResource::from_gvk(&gvk);
    let api: Api<DynamicObject> = if namespace.is_empty() {
        Api::all_with(client.clone(), &ar)
    } else {
        Api::namespaced_with(client.clone(), &namespace, &ar)
    };
    info!(
        group = %group,
        "Starting KMS ZAP controller (additive, zap-native CRs only)"
    );
    let ctx = Arc::new(Ctx { client, group });
    Controller::new_with(api, Config::default(), ar)
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = ?e, "KMS ZAP controller stream error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_spec() -> ZapKmsSpec {
        ZapKmsSpec {
            transport: "zap".into(),
            zap_addr: "kms.hanzo.svc:9000".into(),
            project_slug: "hanzo-goproxy".into(),
            env_slug: "prod".into(),
            secrets_path: "/".into(),
            keys: vec!["GH_PROXY_TOKEN".into()],
            managed_secret_name: "goproxy-secrets".into(),
            allowed_namespaces: vec![],
            cluster_name: "hanzo".into(),
            ..Default::default()
        }
    }

    #[test]
    fn only_transport_zap_is_native() {
        let mut s = base_spec();
        s.transport = String::new();
        assert!(!is_zap_native(&s));
        s.transport = "rest".into();
        assert!(!is_zap_native(&s));
        s.transport = "zap".into();
        assert!(is_zap_native(&s));
    }

    #[test]
    fn validate_spec_accepts_well_formed() {
        assert!(validate_spec(&base_spec()).is_ok());
    }

    #[test]
    fn validate_spec_rejects_non_zap() {
        let mut s = base_spec();
        s.transport = "rest".into();
        assert!(validate_spec(&s).is_err());
    }

    #[test]
    fn validate_spec_rejects_empty_required_fields() {
        let mut a = base_spec();
        a.zap_addr = String::new();
        assert!(validate_spec(&a).is_err());
        let mut b = base_spec();
        b.project_slug = String::new();
        assert!(validate_spec(&b).is_err());
        let mut c = base_spec();
        c.env_slug = String::new();
        assert!(validate_spec(&c).is_err());
        let mut d = base_spec();
        d.managed_secret_name = String::new();
        assert!(validate_spec(&d).is_err());
    }

    #[test]
    fn validate_spec_rejects_empty_or_oversize_keys() {
        let mut empty = base_spec();
        empty.keys = vec![];
        assert!(validate_spec(&empty).is_err());

        let mut long_key = base_spec();
        long_key.keys = vec!["x".repeat(MAX_KEY + 1)];
        assert!(validate_spec(&long_key).is_err());

        let mut too_many = base_spec();
        too_many.keys = (0..(MAX_KEYS + 1)).map(|i| format!("k{i}")).collect();
        assert!(validate_spec(&too_many).is_err());
    }

    #[test]
    fn validate_spec_rejects_oversize_fields() {
        let mut s = base_spec();
        s.zap_addr = "a".repeat(MAX_ADDR + 1);
        assert!(validate_spec(&s).is_err());
        let mut p = base_spec();
        p.secrets_path = "p".repeat(MAX_FIELD + 1);
        assert!(validate_spec(&p).is_err());
    }

    #[test]
    fn namespace_same_ns_allowed_by_default() {
        let s = base_spec();
        assert!(namespace_allowed(&s, "hanzo", "hanzo"));
        assert!(!namespace_allowed(&s, "hanzo", "kube-system"));
    }

    #[test]
    fn namespace_allow_list_honored() {
        let mut s = base_spec();
        s.allowed_namespaces = vec!["lux".into()];
        assert!(namespace_allowed(&s, "hanzo", "lux"));
        assert!(!namespace_allowed(&s, "hanzo", "zoo"));
    }
}

#[cfg(test)]
mod projection_tests {
    use super::*;

    fn spec() -> ZapKmsSpec {
        ZapKmsSpec {
            transport: "zap".into(),
            zap_addr: "cloud.hanzo.svc:9653".into(),
            project_slug: "hanzo".into(),
            env_slug: "prod".into(),
            secrets_path: "/deploy".into(),
            keys: vec!["FORGE_TOKEN".into()],
            managed_secret_name: "forge-token".into(),
            ..Default::default()
        }
    }

    /// The kubelet type-checks a pull secret and skips an Opaque one without
    /// logging a word, which reads back as a bad credential when the credential
    /// was never consulted. So a declared type has to reach the object, and
    /// Opaque stays absent because that is already the default.
    #[test]
    fn a_declared_secret_type_reaches_the_object() {
        assert_eq!(secret_type(&spec()), None);
        let mut s = spec();
        s.secret_type = "Opaque".into();
        assert_eq!(secret_type(&s), None, "Opaque is the default, not a value to set");
        s.secret_type = "kubernetes.io/dockerconfigjson".into();
        assert_eq!(secret_type(&s).as_deref(), Some("kubernetes.io/dockerconfigjson"));
    }

    /// Orphan is the default because deleting a reference must not pull env out
    /// from under a running pod. An owner reference cannot cross a namespace
    /// either, so a cross-namespace projection is Orphan whatever it asked for —
    /// the alternative is a Secret collected the moment it appears.
    #[test]
    fn ownership_defaults_to_orphan_and_never_crosses_a_namespace() {
        assert!(!owned(&spec(), "hanzo", "hanzo"), "unset means Orphan");
        let mut s = spec();
        s.creation_policy = "Orphan".into();
        assert!(!owned(&s, "hanzo", "hanzo"));
        s.creation_policy = "Owner".into();
        assert!(owned(&s, "hanzo", "hanzo"));
        assert!(
            !owned(&s, "hanzo", "other"),
            "Owner across namespaces would be collected immediately"
        );
    }

    #[test]
    fn the_secret_lands_where_the_cr_says() {
        assert_eq!(target_namespace(&spec(), "hanzo"), "hanzo");
        let mut s = spec();
        s.secret_namespace = "hanzo-build".into();
        assert_eq!(target_namespace(&s, "hanzo"), "hanzo-build");
    }

    /// KMS names a value one thing and the consumer reads another. Renaming is
    /// the whole of what the old Go template did, so it is spelled as a map.
    #[test]
    fn renaming_maps_the_secret_key_to_the_kms_key() {
        let mut s = spec();
        s.rename.insert("token".into(), "FORGE_TOKEN".into());
        let fetched = BTreeMap::from([("FORGE_TOKEN".to_string(), "abc".to_string())]);
        let out = apply_rename(&s, fetched).expect("renames");
        assert_eq!(out.get("token").map(String::as_str), Some("abc"));
        assert!(!out.contains_key("FORGE_TOKEN"), "the source name is consumed");
    }

    /// A key the rename does not mention keeps its own name, so naming one
    /// remapping does not silently drop the rest.
    #[test]
    fn unrenamed_keys_are_kept() {
        let mut s = spec();
        s.rename.insert("token".into(), "FORGE_TOKEN".into());
        let fetched = BTreeMap::from([
            ("FORGE_TOKEN".to_string(), "abc".to_string()),
            ("OTHER".to_string(), "xyz".to_string()),
        ]);
        let out = apply_rename(&s, fetched).expect("renames");
        assert_eq!(out.get("token").map(String::as_str), Some("abc"));
        assert_eq!(out.get("OTHER").map(String::as_str), Some("xyz"));
    }

    /// A rename pointing at a key that was never fetched is refused here, where
    /// the reason is legible, rather than producing a Secret missing a key —
    /// which fails at the next pod creation instead.
    #[test]
    fn a_rename_from_a_key_that_was_not_fetched_is_refused() {
        let mut s = spec();
        s.rename.insert("token".into(), "TYPO".into());
        let fetched = BTreeMap::from([("FORGE_TOKEN".to_string(), "abc".to_string())]);
        let err = apply_rename(&s, fetched).expect_err("refuses");
        assert!(err.contains("TYPO"), "{err}");
    }

    /// No rename is the common case and must not disturb anything.
    #[test]
    fn without_a_rename_the_keys_are_untouched() {
        let fetched = BTreeMap::from([("A".to_string(), "1".to_string())]);
        let out = apply_rename(&spec(), fetched.clone()).expect("passes through");
        assert_eq!(out, fetched);
    }
}

#[cfg(test)]
mod literal_tests {
    use super::*;

    fn spec() -> ZapKmsSpec {
        ZapKmsSpec {
            transport: "zap".into(),
            zap_addr: "cloud.hanzo.svc:9653".into(),
            project_slug: "hanzo".into(),
            env_slug: "prod".into(),
            secrets_path: "/cd".into(),
            keys: vec!["forge-token".into()],
            managed_secret_name: "repo-creds".into(),
            ..Default::default()
        }
    }

    /// A repository credential is a KMS token plus the host, the user and the
    /// kind of repo — facts that are not secret and have nowhere else to live.
    /// Dropping them leaves a Secret the consumer cannot use.
    #[test]
    fn literals_land_beside_the_fetched_values() {
        let mut s = spec();
        s.rename.insert("password".into(), "forge-token".into());
        s.literals.insert("url".into(), "https://git.hanzo.ai/".into());
        s.literals.insert("username".into(), "cd".into());
        s.literals.insert("type".into(), "git".into());

        let fetched = BTreeMap::from([("forge-token".to_string(), "tok".to_string())]);
        let mut data = apply_rename(&s, fetched).expect("renames");
        for (k, v) in &s.literals {
            assert!(!data.contains_key(k));
            data.insert(k.clone(), v.clone());
        }
        assert_eq!(data.get("password").map(String::as_str), Some("tok"));
        assert_eq!(data.get("url").map(String::as_str), Some("https://git.hanzo.ai/"));
        assert_eq!(data.get("username").map(String::as_str), Some("cd"));
        assert_eq!(data.get("type").map(String::as_str), Some("git"));
    }

    /// Refetch cadence: the projector's own unless asked, and never so fast that
    /// a refresh becomes a tight loop against KMS.
    #[test]
    fn the_resync_cadence_has_a_default_and_a_floor() {
        assert_eq!(resync(&spec()), Duration::from_secs(300));
        let mut s = spec();
        s.resync_interval = 60;
        assert_eq!(resync(&s), Duration::from_secs(60));
        s.resync_interval = 1;
        assert_eq!(resync(&s), Duration::from_secs(30), "floored");
        s.resync_interval = -5;
        assert_eq!(resync(&s), Duration::from_secs(300), "nonsense falls back");
    }
}
