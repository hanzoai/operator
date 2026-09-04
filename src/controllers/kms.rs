//! KMSSecret projector: KMS values into Kubernetes Secrets.
//!
//! Two transports over ONE projection. `transport: "iam"` reads KMS over its
//! HTTP contract, authenticating as the operator's own IAM client
//! (`KMS_CLIENT_ID`, `KMS_CLIENT_SECRET`, `KMS_URL`); the org is the one that
//! credential belongs to — it rides the token and is never named in a path.
//! `transport: "zap"` reads the same store over the ZAP binary protocol
//! (`crate::zapclient`) and cannot authenticate yet (`_WHY_ZAP_IS_OFF`).
//! Everything after the fetch — rename, literals, secret type, ownership, the
//! hijack guard, the SSA apply — is one path, so a transport is only ever a
//! question of where the bytes come from.
//!
//! One CRD family — the same `kms.<universe>/v1 KMSSecret` the operator already
//! writes (`controllers::service::reconcile_kms_secret`), watched group-erased
//! as a DynamicObject so it works under every universe api-group.
//!
//! Opt-in (`KMS_PROJECTOR=true`) and fail-closed: a CR naming any other
//! transport is IGNORED, and any fetch error means no Secret write — never a
//! plaintext default. Reuses the canonical guards in `core::secret` (strict
//! hijack-protection + control-byte rejection).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::Api;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::Client;
use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::apply;
use crate::core::secret::{
    is_operator_managed, owner_ref, validate_secret_value, MANAGED_BY_LABEL,
};
use crate::zapclient::ZapClient;

/// managed-by value for Secrets this projector owns — distinct from the REST
/// projector's so the two never adopt each other's Secrets.
pub const KMS_MANAGER: &str = "hanzo-operator-kms";

/// Where KMS answers when the environment does not say. Cloud mounts it at
/// `/v1/kms` in-cluster.
const KMS_URL: &str = "http://cloud.hanzo.svc:8000";

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

/// A `KMSSecret` spec, as this projector reads it.
#[derive(Debug, Clone, Deserialize, Default)]
struct Spec {
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
    #[serde(default, rename = "credentialsRef")]
    credentials_ref: String,
}

/// The two ways a CR can ask for its bytes. Anything else is not ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Iam,
    Zap,
}

/// Which transport a CR opts into, if any. `None` means another projector's CR.
fn transport(s: &Spec) -> Option<Transport> {
    match s.transport.as_str() {
        "iam" => Some(Transport::Iam),
        "zap" => Some(Transport::Zap),
        _ => None,
    }
}

/// How often to refetch. Zero means the projector's own cadence; anything under
/// a floor would be a tight loop against KMS rather than a refresh.
fn resync(s: &Spec) -> Duration {
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
fn secret_type(s: &Spec) -> Option<String> {
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
fn owned(s: &Spec, cr_ns: &str, target_ns: &str) -> bool {
    s.creation_policy == "Owner" && cr_ns == target_ns
}

/// Where the Secret is written: the CR's own namespace unless it names another.
fn target_namespace(s: &Spec, cr_ns: &str) -> String {
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
    s: &Spec,
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

/// Why `transport: "zap"` still cannot read, stated where someone will look.
///
/// The KMS ZAP server requires every secret opcode to arrive as a signed
/// Envelope — the caller's mnemonic-derived service NodeID, a 48-byte
/// SHAKE256-384 commitment, and an ML-DSA-65 signature over the canonical
/// digest — and `verifyAndAuthorize` parses it before any store I/O with no
/// permissive path. `zapclient::call` sends `[opcode || bare JSON]` and builds
/// no envelope, so every fetch is rejected at parse. A zap-native CR therefore
/// fails closed on every reconcile: it cannot write a wrong secret, and it
/// cannot write a right one either.
///
/// `transport: "iam"` reads. It answers the same two questions the other way:
/// authentication is a bearer minted from the operator's own IAM client
/// credential, and authorization is the `owner` claim on that bearer, which KMS
/// re-checks on every read. The org rides the token and never the path.
///
/// That is also why `credentialsRef` is refused on BOTH transports. The
/// identity here is the operator's, one per universe, taken from its own
/// environment; a CR that named an identity would be selecting a tenant, and
/// the tenant it selected would be read through a path that looks correct with
/// nothing downstream able to tell.
///
/// What would close zap is the piece it has always been missing:
/// `BuildEnvelope(ident: &ServiceIdentity, op, req, nonce, bind, now)` in
/// luxfi/kms takes the identity as its first argument, and the server
/// authorizes on the verified NodeID. Sign as an identity and zap both
/// authenticates and reads as the right tenant.
///
/// Note also that `OpSecretGet` carries only `{path, name, env}`, and the HTTP
/// read route carries the same three. There is no project field on either wire,
/// so `spec.projectSlug` reaches nothing on either transport: the store is
/// addressed by path, name and env, and the identity is what scopes it.
const _WHY_ZAP_IS_OFF: () = ();

/// True iff every character rides in a URL path segment as itself — no escaping
/// to get wrong, and nothing that could add a segment, a query or a fragment.
fn literal(s: &str, slash: bool) -> bool {
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') || (slash && c == '/'))
}

/// The KMS read route for one key: `secretsPath` "/pkg" + key "htpasswd" is
/// `pkg/htpasswd`, under `/v1/kms/secrets/`. An empty or root path is just the
/// key.
fn route(path: &str, key: &str) -> String {
    let p = path.trim_matches('/');
    if p.is_empty() {
        key.to_string()
    } else {
        format!("{p}/{key}")
    }
}

/// Validate a spec before any I/O. Pure.
fn validate_spec(s: &Spec) -> Result<(), String> {
    let kind = transport(s).ok_or("transport is neither iam nor zap")?;
    if kind == Transport::Zap {
        // The peer to dial. On iam the host is the operator's own KMS_URL: a CR
        // that named it would be aiming the operator's bearer at a host of the
        // CR author's choosing.
        if s.zap_addr.is_empty() || s.zap_addr.len() > MAX_ADDR {
            return Err("zapAddr empty or too long".into());
        }
        if s.project_slug.is_empty() || s.project_slug.len() > MAX_FIELD {
            return Err("projectSlug empty or too long".into());
        }
    }
    if s.env_slug.is_empty() || s.env_slug.len() > MAX_FIELD || !literal(&s.env_slug, false) {
        return Err("envSlug empty, too long, or not a plain name".into());
    }
    if s.secrets_path.len() > MAX_FIELD
        || !literal(&s.secrets_path, true)
        || s.secrets_path.split('/').any(|seg| seg == "..")
    {
        return Err("secretsPath too long, not a plain path, or climbs".into());
    }
    if s.managed_secret_name.is_empty() || s.managed_secret_name.len() > MAX_NAME {
        return Err("managedSecretName empty or too long".into());
    }
    if !s.credentials_ref.is_empty() {
        return Err(format!(
            "credentialsRef {:?} selects a KMS identity, and this projector reads as the operator's own — one IAM client per universe, from its own environment; refusing rather than reading whichever tenant that identity resolves to",
            s.credentials_ref
        ));
    }
    if s.keys.is_empty() || s.keys.len() > MAX_KEYS {
        return Err("keys empty or too many".into());
    }
    for k in &s.keys {
        if k.is_empty() || k.len() > MAX_KEY || !literal(k, false) {
            return Err("key empty, too long, or not a plain name".into());
        }
    }
    Ok(())
}

/// Namespace allow-list: target must be the CR's own namespace, or explicitly
/// allow-listed. No cross-namespace by default.
fn namespace_allowed(s: &Spec, cr_ns: &str, target_ns: &str) -> bool {
    target_ns == cr_ns || s.allowed_namespaces.iter().any(|n| n == target_ns)
}

/// Where one key's bytes come from. The projection reads through this and
/// nothing else, so a transport is a fetcher and never a second code path.
#[async_trait::async_trait]
trait Source: Send {
    async fn get(&mut self, path: &str, key: &str, env: &str) -> Result<String, String>;
}

#[async_trait::async_trait]
impl Source for ZapClient {
    async fn get(&mut self, path: &str, key: &str, env: &str) -> Result<String, String> {
        self.get_secret(path, key, env)
            .await
            .map_err(|e| format!("zap get {key}: {e}"))
    }
}

#[async_trait::async_trait]
impl Source for &Kms {
    async fn get(&mut self, path: &str, key: &str, env: &str) -> Result<String, String> {
        self.read(path, key, env).await
    }
}

/// A bearer and the moment it stops being usable.
struct Bearer {
    value: String,
    until: Instant,
}

/// The KMS reader: one IAM client identity for the whole operator, one cached
/// bearer, one read route.
struct Kms {
    http: reqwest::Client,
    base: String,
    id: String,
    secret: String,
    bearer: Mutex<Option<Bearer>>,
}

/// A bearer is re-minted this long before KMS says it expires, so a token never
/// dies mid-fetch, and is never held for less than the floor.
const MARGIN: u64 = 60;
const MIN_TTL: u64 = 30;

#[derive(Deserialize)]
struct Login {
    #[serde(rename = "accessToken")]
    access_token: String,
    #[serde(rename = "expiresIn", default)]
    expires_in: i64,
}

#[derive(Deserialize)]
struct Fetched {
    value: String,
}

impl Kms {
    /// The operator's identity, from the operator's environment. Both halves of
    /// the credential are required: a projector that cannot authenticate has no
    /// honest reduced mode, it just fails every fetch.
    fn from_env() -> Result<Self, String> {
        let base = std::env::var("KMS_URL").unwrap_or_default();
        let base = if base.trim().is_empty() {
            KMS_URL.to_string()
        } else {
            base
        };
        let id = std::env::var("KMS_CLIENT_ID").unwrap_or_default();
        let secret = std::env::var("KMS_CLIENT_SECRET").unwrap_or_default();
        if id.is_empty() || secret.is_empty() {
            return Err(
                "KMS_CLIENT_ID and KMS_CLIENT_SECRET are required: the projector reads KMS as an IAM client".into(),
            );
        }
        Self::new(&base, &id, &secret)
    }

    fn new(base: &str, id: &str, secret: &str) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .user_agent("hanzo-operator")
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| format!("kms client: {e}"))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            id: id.to_string(),
            secret: secret.to_string(),
            bearer: Mutex::new(None),
        })
    }

    /// The cached bearer, minted on first use. `stale` names the token that just
    /// came back 401: if the cache already holds a different one, another fetch
    /// re-minted it and that one is used instead of logging in again.
    async fn bearer(&self, stale: Option<&str>) -> Result<String, String> {
        let mut held = self.bearer.lock().await;
        if let Some(b) = held.as_ref() {
            let fresh = Instant::now() < b.until;
            let superseded = stale.is_some_and(|s| s != b.value);
            if fresh && (stale.is_none() || superseded) {
                return Ok(b.value.clone());
            }
        }
        let (value, ttl) = self.login().await?;
        *held = Some(Bearer {
            value: value.clone(),
            until: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(value)
    }

    /// Exchange the client credential for a bearer. KMS brokers it at IAM and
    /// returns IAM's own JWT, whose `owner` claim is the org every read is then
    /// scoped to.
    async fn login(&self) -> Result<(String, u64), String> {
        let resp = self
            .http
            .post(format!("{}/v1/kms/auth/login", self.base))
            .json(&serde_json::json!({"clientId": self.id, "clientSecret": self.secret}))
            .send()
            .await
            .map_err(|e| format!("kms login: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("kms login as {}: HTTP {status}", self.id));
        }
        let body: Login = resp
            .json()
            .await
            .map_err(|e| format!("kms login response: {e}"))?;
        if body.access_token.is_empty() {
            return Err(format!("kms login as {}: no token in a 200", self.id));
        }
        let ttl = u64::try_from(body.expires_in).unwrap_or(0);
        Ok((body.access_token, ttl.saturating_sub(MARGIN).max(MIN_TTL)))
    }

    /// Read one key. A 401 re-mints the bearer and retries ONCE; a second 401 is
    /// an authorization answer, not a stale token, so it fails closed.
    async fn read(&self, path: &str, key: &str, env: &str) -> Result<String, String> {
        let route = route(path, key);
        // The route and the env ride the URL as themselves. `validate_spec`
        // already refused anything else; checking again where the URL is built
        // is what makes the escape impossible rather than merely unlikely.
        if !literal(&route, true) || !literal(env, false) {
            return Err(format!(
                "kms get {key}: {route} or {env} is not a plain name"
            ));
        }
        let url = format!("{}/v1/kms/secrets/{route}?env={env}", self.base);
        let token = self.bearer(None).await?;
        let mut resp = self.send(&url, &token).await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            let fresh = self.bearer(Some(&token)).await?;
            resp = self.send(&url, &fresh).await?;
        }
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(format!("kms has no {key} at {route} in {env}"));
        }
        if !status.is_success() {
            return Err(format!("kms get {key} at {route} in {env}: HTTP {status}"));
        }
        let body: Fetched = resp
            .json()
            .await
            .map_err(|e| format!("kms get {key} at {route}: {e}"))?;
        Ok(body.value)
    }

    async fn send(&self, url: &str, token: &str) -> Result<reqwest::Response, String> {
        self.http
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("kms get {url}: {e}"))
    }
}

/// Fetch every key the CR names. Fail-closed: the first error aborts, so a
/// partial read never becomes a partial Secret.
async fn fetch(src: &mut dyn Source, s: &Spec) -> Result<BTreeMap<String, String>, String> {
    let mut data = BTreeMap::new();
    for key in &s.keys {
        let val = src.get(&s.secrets_path, key, &s.env_slug).await?;
        validate_secret_value(val.as_bytes())?;
        data.insert(key.clone(), val);
    }
    Ok(data)
}

struct Ctx {
    client: Client,
    /// Resolved `kms.<universe>` group, for the ownerRef on projected Secrets.
    group: String,
    kms: Arc<Kms>,
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

    // Decode the spec; IGNORE CRs naming another transport.
    let spec: Spec = obj
        .data
        .get("spec")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| format!("spec decode: {e}"))?
        .unwrap_or_default();
    let Some(kind) = transport(&spec) else {
        return Ok(Action::await_change());
    };
    validate_spec(&spec)?;

    // The managed Secret lives in the CR's namespace.
    let target_ns = target_namespace(&spec, &cr_ns);
    if !namespace_allowed(&spec, &cr_ns, &target_ns) {
        return Err(format!("namespace {target_ns} not allowed").into());
    }

    let mut src: Box<dyn Source + '_> = match kind {
        Transport::Iam => Box::new(ctx.kms.as_ref()),
        Transport::Zap => Box::new(
            ZapClient::connect(&spec.zap_addr, &spec.cluster_name)
                .await
                .map_err(|e| format!("zap connect {}: {e}", spec.zap_addr))?,
        ),
    };
    let data = fetch(src.as_mut(), &spec).await?;

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

    // Strict hijack guard: never overwrite a Secret we don't own.
    let secrets: Api<Secret> = Api::namespaced(ctx.client.clone(), &target_ns);
    let cr_uid = obj.metadata.uid.clone().unwrap_or_default();
    if let Some(existing) = secrets
        .get_opt(&spec.managed_secret_name)
        .await
        .map_err(|e| format!("get secret: {e}"))?
    {
        if !is_operator_managed(&existing, KMS_MANAGER, &cr_uid) {
            return Err(format!(
                "refuse to overwrite unmanaged Secret {target_ns}/{}",
                spec.managed_secret_name
            )
            .into());
        }
    }

    // Project via SSA. managed-by label + ownerRef so only we adopt it.
    let mut labels = BTreeMap::new();
    labels.insert(MANAGED_BY_LABEL.to_string(), KMS_MANAGER.to_string());
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
        transport = ?kind,
        keys = spec.keys.len(),
        "KMSSecret reconciled"
    );
    Ok(Action::requeue(resync(&spec)))
}

fn on_error(_obj: Arc<DynamicObject>, err: &ReconcileError, _ctx: Arc<Ctx>) -> Action {
    warn!(error = %err, "KMS projector reconcile error");
    Action::requeue(Duration::from_secs(30))
}

/// Opt-in entrypoint. When `enabled` is false this returns immediately — the
/// projector never watches anything, so the REST projector stays the only
/// secret path until a deliberate cutover. Matches the call shape of the
/// other controllers so it slots into `run_all_controllers`' `join!`.
pub async fn run_kms_projector(client: Client, namespace: String, enabled: bool, api_group: &str) {
    if !enabled {
        info!("KMS projector disabled (set KMS_PROJECTOR=true to enable)");
        return;
    }
    let kms = match Kms::from_env() {
        Ok(k) => Arc::new(k),
        Err(e) => {
            // Starting without a credential would watch CRs and fail every one
            // of them, which reads exactly like a KMS outage.
            error!(error = %e, "KMS projector not started");
            return;
        }
    };
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
        kms = %kms.base,
        "Starting KMS projector (iam and zap transports)"
    );
    let ctx = Arc::new(Ctx { client, group, kms });
    Controller::new_with(api, Config::default(), ar)
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = ?e, "KMS projector stream error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_spec() -> Spec {
        Spec {
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
    fn only_iam_and_zap_are_ours() {
        let mut s = base_spec();
        s.transport = String::new();
        assert_eq!(transport(&s), None);
        s.transport = "rest".into();
        assert_eq!(transport(&s), None);
        s.transport = "zap".into();
        assert_eq!(transport(&s), Some(Transport::Zap));
        s.transport = "iam".into();
        assert_eq!(transport(&s), Some(Transport::Iam));
    }

    #[test]
    fn validate_spec_accepts_well_formed() {
        assert!(validate_spec(&base_spec()).is_ok());
    }

    #[test]
    fn validate_spec_rejects_another_projectors_cr() {
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

    /// The host and the project are the zap transport's, not the projector's.
    /// On iam the host is the operator's own `KMS_URL` — a CR that named it
    /// would aim the operator's bearer at a host of the CR author's choosing —
    /// and the project reaches nothing on either wire.
    #[test]
    fn an_iam_cr_names_neither_a_host_nor_a_project() {
        let mut s = base_spec();
        s.transport = "iam".into();
        s.zap_addr = String::new();
        s.project_slug = String::new();
        assert!(validate_spec(&s).is_ok());
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

    /// A path and a key become URL path segments on the iam transport. A key
    /// carrying `?`, `/` or a `..` segment would address a secret other than the
    /// one the CR names, so it is refused before any request is built.
    #[test]
    fn a_key_or_path_that_could_address_something_else_is_refused() {
        let mut q = base_spec();
        q.keys = vec!["tok?env=prod".into()];
        assert!(validate_spec(&q).is_err());
        let mut slash = base_spec();
        slash.keys = vec!["pkg/htpasswd".into()];
        assert!(validate_spec(&slash).is_err());
        let mut climb = base_spec();
        climb.secrets_path = "/pkg/../other".into();
        assert!(validate_spec(&climb).is_err());
        let mut env = base_spec();
        env.env_slug = "prod&x=1".into();
        assert!(validate_spec(&env).is_err());
    }

    /// KMS addresses a secret by path and name: `/pkg` + `htpasswd` reads
    /// `/v1/kms/secrets/pkg/htpasswd`. A root or empty path is just the key.
    #[test]
    fn the_read_route_is_the_path_joined_to_the_key() {
        assert_eq!(route("/pkg", "htpasswd"), "pkg/htpasswd");
        assert_eq!(route("pkg/", "htpasswd"), "pkg/htpasswd");
        assert_eq!(route("/", "htpasswd"), "htpasswd");
        assert_eq!(route("", "htpasswd"), "htpasswd");
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

    fn spec() -> Spec {
        Spec {
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

    fn spec() -> Spec {
        Spec {
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

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            transport: "iam".into(),
            env_slug: "prod".into(),
            secrets_path: "/lux-chat".into(),
            keys: vec!["JWT_SECRET".into()],
            managed_secret_name: "lux-chat-env".into(),
            ..Default::default()
        }
    }

    /// The identity is the operator's, one IAM client per universe, read from
    /// its own environment. A CR naming one would be choosing a tenant — the
    /// org rides the bearer, so the material returned would be that tenant's
    /// through a path that looks exactly right, and nothing downstream could
    /// tell. Refusing is the only honest answer: a wrong secret that applies
    /// cleanly is worse than one that never arrives.
    #[test]
    fn a_cr_that_selects_a_tenant_by_identity_is_refused() {
        let mut s = spec();
        s.credentials_ref = "lux-chat-iam-creds".into();
        let err = validate_spec(&s).expect_err("must refuse");
        assert!(err.contains("lux-chat-iam-creds"), "{err}");
        assert!(err.contains("operator's own"), "names why: {err}");
    }

    /// Everything else still passes — the refusal is scoped to the one field
    /// that would move the read to another tenant.
    #[test]
    fn a_cr_without_an_identity_is_served() {
        assert!(validate_spec(&spec()).is_ok());
    }
}

/// The iam transport against the two routes KMS serves, in process.
#[cfg(test)]
mod iam_tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Fake {
        logins: AtomicUsize,
        reads: AtomicUsize,
        /// Answer every read 401, whatever bearer it carries.
        refuse: bool,
    }

    /// Each login mints a distinct token, so a cached read and a re-minted one
    /// are distinguishable at the assertion.
    async fn login(State(f): State<Arc<Fake>>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let n = f.logins.fetch_add(1, Ordering::SeqCst) + 1;
        assert_eq!(body["clientId"], "hanzo-operator");
        assert_eq!(body["clientSecret"], "s3cret");
        Json(serde_json::json!({
            "accessToken": format!("token-{n}"),
            "expiresIn": 3600,
            "tokenType": "Bearer",
        }))
    }

    async fn read(
        Path(route): Path<String>,
        Query(q): Query<HashMap<String, String>>,
        headers: HeaderMap,
        State(f): State<Arc<Fake>>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        f.reads.fetch_add(1, Ordering::SeqCst);
        let bearer = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if f.refuse || !bearer.starts_with("Bearer token-") {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
        }
        if route.ends_with("absent") {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({})));
        }
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "env": q.get("env").cloned().unwrap_or_default(),
                "name": route,
                "value": format!("{route}@{}", q.get("env").cloned().unwrap_or_default()),
            })),
        )
    }

    /// Serve the two routes on an ephemeral port; returns a Kms pointed at them.
    async fn serve(f: Arc<Fake>) -> Kms {
        let app = Router::new()
            .route("/v1/kms/auth/login", post(login))
            .route("/v1/kms/secrets/{*route}", get(read))
            .with_state(f);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app.into_make_service()).await;
        });
        Kms::new(&format!("http://{addr}"), "hanzo-operator", "s3cret").expect("client")
    }

    fn spec() -> Spec {
        Spec {
            transport: "iam".into(),
            env_slug: "prod".into(),
            secrets_path: "/pkg".into(),
            keys: vec!["htpasswd".into()],
            managed_secret_name: "pkg-auth".into(),
            ..Default::default()
        }
    }

    /// One login serves every read until the token expires. The operator holds
    /// hundreds of CRs; logging in per key would turn a resync into a
    /// credential-stuffing pattern against IAM.
    #[tokio::test]
    async fn the_bearer_is_minted_once_and_reused() {
        let f = Arc::new(Fake::default());
        let kms = serve(f.clone()).await;
        assert_eq!(kms.read("/pkg", "htpasswd", "prod").await.unwrap(), "pkg/htpasswd@prod");
        assert_eq!(kms.read("/pkg", "other", "prod").await.unwrap(), "pkg/other@prod");
        assert_eq!(f.logins.load(Ordering::SeqCst), 1, "one login for two reads");
        assert_eq!(f.reads.load(Ordering::SeqCst), 2, "no retries");
    }

    /// A 401 is first read as an expired token: re-mint once and try again. A
    /// second 401 is an answer about authority, so it fails closed rather than
    /// looping on IAM.
    #[tokio::test]
    async fn a_401_re_mints_once_then_fails_closed() {
        let f = Arc::new(Fake {
            refuse: true,
            ..Default::default()
        });
        let kms = serve(f.clone()).await;
        let err = kms.read("/pkg", "htpasswd", "prod").await.expect_err("fails closed");
        assert!(err.contains("401"), "{err}");
        assert_eq!(f.logins.load(Ordering::SeqCst), 2, "one login, one re-mint");
        assert_eq!(f.reads.load(Ordering::SeqCst), 2, "one retry, not a loop");
    }

    /// A key KMS does not hold names itself in the error. The alternative is a
    /// Secret that applies without it and a pod that fails on the env var.
    #[tokio::test]
    async fn a_missing_key_fails_closed_and_names_itself() {
        let f = Arc::new(Fake::default());
        let kms = serve(f.clone()).await;
        let err = kms.read("/pkg", "absent", "prod").await.expect_err("fails closed");
        assert!(err.contains("absent"), "names the key: {err}");
        assert!(err.contains("prod"), "names the env: {err}");
    }

    /// The whole projection over the iam fetcher: what KMS returns for a key
    /// lands under the Secret key the CR asked for, rename included.
    #[tokio::test]
    async fn a_fetched_value_lands_under_the_secret_key() {
        let kms = serve(Arc::new(Fake::default())).await;
        let mut s = spec();
        s.rename.insert("auth".into(), "htpasswd".into());
        let fetched = fetch(&mut &kms, &s).await.expect("fetches");
        let data = apply_rename(&s, fetched).expect("renames");
        assert_eq!(data.get("auth").map(String::as_str), Some("pkg/htpasswd@prod"));
        assert!(!data.contains_key("htpasswd"));
    }

    /// One failure aborts the whole fetch: a Secret carrying half a CR's keys is
    /// worse than one that never appears, because the pod starts.
    #[tokio::test]
    async fn one_missing_key_abandons_the_whole_fetch() {
        let kms = serve(Arc::new(Fake::default())).await;
        let mut s = spec();
        s.keys = vec!["htpasswd".into(), "absent".into()];
        assert!(fetch(&mut &kms, &s).await.is_err());
    }
}
