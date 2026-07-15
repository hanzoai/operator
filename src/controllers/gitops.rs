//! Native git → CR reconcile — the continuous git→cluster apply loop, folded
//! into the operator process. Replaces the external `gitops-reconcile` CronJob
//! (`hanzoai/universe/infra/k8s/gitops-reconcile`) so the operator does the
//! whole chain natively: **git → CR → workload** (this loop applies the CR; the
//! per-Kind controllers already turn the CR into a Deployment/Service/etc.).
//!
//! ## Why this is not a `kube::Controller`
//!
//! Every CRD controller in this binary watches objects that already exist in
//! *this* cluster. This loop's source is *git* — `infra/k8s/operator/crs/*.yaml`
//! on the `hanzoai/universe` main branch. So, like the `apps` controller, it is
//! a periodic poll (with an optional webhook nudge), not a watch: each tick
//! pulls the declared CRs from git and server-side-applies the in-scope ones.
//!
//! ## What it internalizes from `reconcile.sh` (the proven stopgap it replaces)
//!
//! * **Source** — `infra/k8s/operator/crs/*.yaml` on `hanzoai/universe` main,
//!   read with a token from a KMS-synced secret mounted at `/creds/token`
//!   (identical credential + never logged; see [`Token`]).
//! * **Apply** — server-side apply (force-conflicts) each in-scope CR under a
//!   distinct field manager. Kind-agnostic: whatever `apiVersion`/`kind` each
//!   file declares is applied (Service today, App tomorrow, plus KMSSecret /
//!   Ingress / PVC — all one path).
//! * **NEVER prune** — a CR removed from git is left alone. This is enforced by
//!   the type system: [`Plan`] can only describe applies; there is no delete
//!   path anywhere in this module.
//! * **Drift report** — every sweep logs a per-object create/update/in-sync
//!   line plus a summary (the "what changed" report).
//!
//! ## What it fixes vs the CronJob
//!
//! * **Real-time, not 5-min cron** — a tight resync loop (default 45s,
//!   `GITOPS_RESYNC_SECS`) is the baseline; a `POST /reconcile` webhook triggers
//!   an *instant* sweep (git push → reconcile) with the poll as the guaranteed
//!   fallback.
//! * **Ownership scope, not a hand-maintained allow-list** — the default scope
//!   is every platform CR in the namespaces this loop owns (`{hanzo}`), by
//!   ownership, not the CronJob's 24-item string list. `GITOPS_NAMESPACES` widens
//!   the owned SET one namespace at a time (`*` = every namespace — the
//!   documented end-state); `GITOPS_APPLY_SCOPE` optionally narrows to a vetted
//!   subset of CR refs for a cautious rollout, then widens to empty.
//!
//! ## Fail-safe by construction
//!
//! Opt-in (`GITOPS_RECONCILE_ENABLED=true`, default off) and additive — it runs
//! ALONGSIDE the CR→workload controllers and never blocks them. A source failure
//! (clone/list error, bad YAML, missing token) is logged and retried next tick;
//! it never crashes the operator (the loop never `?`-propagates out of its body,
//! never `unwrap`s, never panics).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kube::api::Api;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind, TypeMeta};
use kube::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::apply;
use crate::core::OperatorError;

/// SSA field manager for git-sourced applies — distinct from the CR
/// controllers' `hanzo-operator` so a git-driven apply is attributable and never
/// silently fights a per-Kind reconcile over the same field.
const FIELD_MANAGER: &str = "hanzo-operator-gitops";

const DEFAULT_REPO: &str = "hanzoai/universe";
const DEFAULT_BRANCH: &str = "main";
const DEFAULT_CRS_PATH: &str = "infra/k8s/operator/crs";
const DEFAULT_NAMESPACE: &str = "hanzo";
const DEFAULT_API_BASE: &str = "https://api.github.com";
const DEFAULT_TOKEN_FILE: &str = "/creds/token";

/// The explicit "every namespace" marker for `GITOPS_NAMESPACES` — the same `*`
/// idiom `APPS_DRIVE_ALLOW` uses. ONLY this reaches the end-state: a blank or
/// absent value keeps the default, so owning the whole cluster is always a
/// deliberate act and never a stray `value: ""`.
const ALL: &str = "*";

/// Entry separators, shared by `GITOPS_NAMESPACES` and `GITOPS_APPLY_SCOPE` —
/// one grammar for both lists.
const SEPS: [char; 4] = [',', ' ', '\t', '\n'];

/// Baseline poll cadence. Far tighter than the 5-min CronJob it replaces; the
/// webhook makes the common case instant, this is the safety floor.
const DEFAULT_RESYNC_SECS: u64 = 45;
/// Floor on the resync interval — a misconfigured tiny value can't hammer the
/// git host.
const MIN_RESYNC_SECS: u64 = 10;

/// The kustomize entrypoint is metadata, not a CR — skipped exactly like
/// `reconcile.sh`'s `[ "$b" = "kustomization.yaml" ] && continue`.
const KUSTOMIZATION: &str = "kustomization.yaml";

// ---------------------------------------------------------------------------
// Credential — a git read token, wrapped so it can never land in a log line.
// ---------------------------------------------------------------------------

/// A git read credential. The inner value is reachable only via [`Token::expose`]
/// at the single call site that sets the `Authorization` header; `Debug`
/// redacts, so a `?token` or `error = %e` can never leak it. (The token travels
/// as a header, never in a URL, so there is no URL form to scrub either.)
#[derive(Clone)]
struct Token(String);

impl Token {
    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(***)")
    }
}

// ---------------------------------------------------------------------------
// Scope — ownership-based, replacing the hand-maintained allow-list.
// ---------------------------------------------------------------------------

/// One entry of the vetted subset (`GITOPS_APPLY_SCOPE`). `zen/zen` pins the
/// (namespace, name) pair; a bare `zen` matches that name in ANY owned namespace.
///
/// A CR's identity is (namespace, name), never a bare name: once more than one
/// namespace is owned the same name can exist in two of them, so the pinned form
/// is the only way to name exactly one CR. The namespace gate runs first, so a
/// bare entry can never widen past [`Scope::namespaces`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Ref {
    namespace: Option<String>,
    name: String,
}

impl Ref {
    /// Parse one entry: `namespace/name` (pinned) or `name` (any owned
    /// namespace). `None` for a malformed entry — an empty side, or more than
    /// one `/`.
    fn parse(raw: &str) -> Option<Self> {
        let raw = raw.trim();
        if raw.is_empty() {
            return None;
        }
        match raw.split_once('/') {
            None => Some(Self {
                namespace: None,
                name: raw.to_string(),
            }),
            Some((ns, name)) if !ns.is_empty() && !name.is_empty() && !name.contains('/') => {
                Some(Self {
                    namespace: Some(ns.to_string()),
                    name: name.to_string(),
                })
            }
            Some(_) => None,
        }
    }

    /// Does this entry vet `(namespace, name)`? A bare entry ignores the
    /// namespace — the namespace gate already ran; a pinned entry must match both.
    fn matches(&self, namespace: &str, name: &str) -> bool {
        self.name == name && self.namespace.as_deref().is_none_or(|ns| ns == namespace)
    }
}

/// Why an object is out of scope. Carried on the skip so the drift report can say
/// WHICH CR is unreconciled and WHY — a bare count is what let a CR sit
/// un-reconciled for 28h behind `skipped=1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// Its namespace is not one this loop owns (`GITOPS_NAMESPACES`). An object
    /// declaring NO namespace lands here too: this loop owns namespaced objects.
    Namespace,
    /// Its name is not in the vetted subset (`GITOPS_APPLY_SCOPE`).
    Name,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Reason::Namespace => "namespace not owned by this loop (widen GITOPS_NAMESPACES)",
            Reason::Name => "name not in the vetted subset (widen GITOPS_APPLY_SCOPE)",
        })
    }
}

/// Which CRs this loop is permitted to apply. A CR is in-scope iff it declares a
/// namespace, AND ([`Scope::namespaces`] is EMPTY — every namespace — OR contains
/// that namespace), AND (the optional [`Scope::names`] subset is absent, OR vets
/// the CR's (namespace, name)).
///
/// The two gates are orthogonal and compose in one direction: the namespace gate
/// runs first, so the name subset can only narrow WITHIN the owned namespaces.
///
/// The DEFAULT is ownership-by-namespace over `{hanzo}` — every platform CR in
/// the operator's namespace, no string list to maintain. `GITOPS_NAMESPACES`
/// widens the owned set (`*` = every namespace, the end-state); `GITOPS_APPLY_SCOPE`
/// narrows to a vetted subset for a cautious rollout; clear it once trusted.
///
/// NOT `Default` on purpose: the derived default would be the EMPTY set, which
/// means every namespace — a fail-open scope reachable by a stray `..Default`.
/// The only constructor takes a resolved set from [`resolve_namespaces`].
#[derive(Clone, Debug)]
struct Scope {
    /// Owned namespaces. EMPTY = ALL namespaces — the same `empty ⇒ all` idiom
    /// the operator's `WATCH_NAMESPACE`/`Api::all` controllers already use.
    namespaces: HashSet<String>,
    names: Option<HashSet<Ref>>,
}

impl Scope {
    /// Build from a resolved namespace set + the raw `GITOPS_APPLY_SCOPE` value
    /// (comma/whitespace-separated refs). Empty → `None` = every name in the
    /// owned namespaces. A malformed entry is dropped LOUDLY — a silently
    /// narrower scope is the exact failure this loop must not have.
    fn new(namespaces: HashSet<String>, raw_scope: &str) -> Self {
        let mut names = HashSet::new();
        for entry in raw_scope.split(SEPS).map(str::trim).filter(|s| !s.is_empty()) {
            match Ref::parse(entry) {
                Some(r) => {
                    names.insert(r);
                }
                None => warn!(
                    %entry,
                    "gitops: ignoring malformed GITOPS_APPLY_SCOPE entry (want `name` or `namespace/name`)"
                ),
            }
        }
        Self {
            namespaces,
            names: if names.is_empty() { None } else { Some(names) },
        }
    }

    /// Decide one object: `Ok(())` = in scope, `Err(reason)` = out of scope, with
    /// the reason the drift report prints. Pure — the load-bearing safety gate,
    /// unit-tested without a cluster.
    fn decide(&self, obj: &DynamicObject) -> Result<(), Reason> {
        // This loop owns NAMESPACED objects. An object declaring no namespace is
        // out of scope in EVERY configuration — including the `*` end-state,
        // where the namespace gate itself admits everything. There is no implied
        // namespace to apply it into.
        let namespace = obj.metadata.namespace.as_deref().unwrap_or_default();
        if namespace.is_empty() {
            return Err(Reason::Namespace);
        }
        if !self.namespaces.is_empty() && !self.namespaces.contains(namespace) {
            return Err(Reason::Namespace);
        }
        let name = obj.metadata.name.as_deref().unwrap_or_default();
        match &self.names {
            None => Ok(()),
            Some(set) if set.iter().any(|r| r.matches(namespace, name)) => Ok(()),
            Some(_) => Err(Reason::Name),
        }
    }
}

/// Resolve the owned namespace set. Precedence: `GITOPS_NAMESPACES` (plural,
/// comma-separated; `*` = every namespace) beats the singular `GITOPS_NAMESPACE`,
/// which beats the default `{hanzo}`.
///
/// Absent — or blank, or all-separators — at every level yields `{hanzo}`, so the
/// widening is a NO-OP until deliberately configured. The empty set (every
/// namespace) is reachable ONLY via an explicit `*`: an accidental `value: ""`
/// must never hand this loop the whole cluster.
///
/// Pure over its inputs rather than over the process env, so it is unit-testable
/// without `set_var` (which no test may use: env is process-global and the test
/// harness is threaded).
fn resolve_namespaces(plural: Option<&str>, singular: Option<&str>) -> HashSet<String> {
    if let Some(raw) = plural.map(str::trim).filter(|s| !s.is_empty()) {
        let mut set = HashSet::new();
        for entry in raw.split(SEPS).map(str::trim).filter(|s| !s.is_empty()) {
            if entry == ALL {
                return HashSet::new();
            }
            set.insert(entry.to_string());
        }
        if !set.is_empty() {
            return set;
        }
    }
    if let Some(ns) = singular.map(str::trim).filter(|s| !s.is_empty()) {
        return HashSet::from([ns.to_string()]);
    }
    HashSet::from([DEFAULT_NAMESPACE.to_string()])
}

/// Render the owned set for a log line. The empty set is ALL namespaces and must
/// SAY so — a blank field would read as "none". Sorted, so the line is stable
/// (`HashSet` iteration order is not).
fn namespaces_display(set: &HashSet<String>) -> String {
    if set.is_empty() {
        return format!("{ALL} (every namespace)");
    }
    let mut v: Vec<&str> = set.iter().map(String::as_str).collect();
    v.sort_unstable();
    v.join(",")
}

// ---------------------------------------------------------------------------
// Pure source helpers — file selection, manifest parse, GVK, planning.
// ---------------------------------------------------------------------------

/// True iff `name` names a CR file we reconcile: a `.yaml` that is not the
/// kustomize entrypoint. Mirrors `reconcile.sh`'s per-file selection.
fn is_reconcilable_file(name: &str) -> bool {
    name.ends_with(".yaml") && name != KUSTOMIZATION
}

/// Parse a (possibly multi-document) YAML manifest into DynamicObjects.
///
/// Kind-agnostic: whatever `apiVersion`/`kind` each document declares becomes the
/// object's type — Service today, App tomorrow, KMSSecret / Ingress / PVC all
/// the same path. Empty documents (a bare `---`) and documents with no
/// `apiVersion`/`kind` are skipped. Routed YAML → JSON value → `DynamicObject`
/// so the object's free-form `data` (a `serde_json::Value`) is built natively.
fn parse_manifests(yaml: &str) -> Result<Vec<DynamicObject>, String> {
    let mut out = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(yaml) {
        let yv = serde_yaml::Value::deserialize(doc).map_err(|e| format!("yaml parse: {e}"))?;
        if yv.is_null() {
            continue;
        }
        let jv: serde_json::Value =
            serde_json::to_value(&yv).map_err(|e| format!("yaml→json: {e}"))?;
        let obj: DynamicObject =
            serde_json::from_value(jv).map_err(|e| format!("→DynamicObject: {e}"))?;
        // A document with no type (e.g. comment-only) is not an object.
        if gvk_of(&obj).is_none() {
            continue;
        }
        out.push(obj);
    }
    Ok(out)
}

/// Extract the object's `GroupVersionKind` from its `apiVersion`/`kind`. Pure.
/// `hanzo.ai/v1` → group `hanzo.ai`; a core `v1` → empty group. `None` when the
/// object carries no usable type.
fn gvk_of(obj: &DynamicObject) -> Option<GroupVersionKind> {
    let tm: &TypeMeta = obj.types.as_ref()?;
    if tm.api_version.is_empty() || tm.kind.is_empty() {
        return None;
    }
    GroupVersionKind::try_from(tm).ok()
}

/// A reason an object was NOT applied — the drift report's skip lines. None of
/// these is a delete: out-of-scope / unparseable / typeless objects are LEFT
/// ALONE, never removed.
#[derive(Debug, PartialEq, Eq)]
enum Skip {
    NotReconcilable {
        file: String,
    },
    Unparseable {
        file: String,
        err: String,
    },
    /// A CR this loop does not own. Carries the FULL (namespace, name) identity
    /// plus the reason: this skip is what silently un-reconciled an app for 28h,
    /// so it must name the CR, not just count it.
    OutOfScope {
        file: String,
        namespace: String,
        name: String,
        reason: Reason,
    },
}

/// The full plan for one sweep: the ordered set of applies, plus the skips (for
/// the drift report). There is deliberately NO delete/prune variant — "never
/// prune" is a property of this type, not a runtime check. A CR removed from git
/// simply stops appearing in `files`, so it produces no plan entry at all and is
/// left untouched in the cluster.
#[derive(Debug, Default)]
struct Plan {
    applies: Vec<(GroupVersionKind, DynamicObject)>,
    skips: Vec<Skip>,
}

/// Turn the source files `(filename, content)` into a [`Plan`]. Pure: file
/// selection + parse + scope + GVK, no I/O, no deletes. This is the reconcile
/// brain and the primary unit-test target.
fn plan(files: &[(String, String)], scope: &Scope) -> Plan {
    let mut plan = Plan::default();
    for (name, content) in files {
        if !is_reconcilable_file(name) {
            plan.skips
                .push(Skip::NotReconcilable { file: name.clone() });
            continue;
        }
        let objs = match parse_manifests(content) {
            Ok(o) => o,
            Err(err) => {
                plan.skips.push(Skip::Unparseable {
                    file: name.clone(),
                    err,
                });
                continue;
            }
        };
        for obj in objs {
            // gvk_of is Some here — parse_manifests filtered out typeless docs.
            let Some(gvk) = gvk_of(&obj) else { continue };
            if let Err(reason) = scope.decide(&obj) {
                plan.skips.push(Skip::OutOfScope {
                    file: name.clone(),
                    namespace: obj.metadata.namespace.clone().unwrap_or_default(),
                    name: obj.metadata.name.clone().unwrap_or_default(),
                    reason,
                });
                continue;
            }
            plan.applies.push((gvk, obj));
        }
    }
    plan
}

/// Decode a GitHub blob's base64 `content` — GitHub wraps it at 60 chars with
/// `\n`, which the standard alphabet rejects, so strip whitespace first. Pure.
fn decode_base64_blob(b64: &str) -> Result<String, String> {
    use base64::Engine;
    let cleaned: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map_err(|e| format!("base64: {e}"))?;
    String::from_utf8(bytes).map_err(|e| format!("utf8: {e}"))
}

/// Parse the resync interval (`GITOPS_RESYNC_SECS`), floored at [`MIN_RESYNC_SECS`],
/// defaulting to [`DEFAULT_RESYNC_SECS`] when unset/unparseable. Pure.
fn parse_resync(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|n| n.max(MIN_RESYNC_SECS))
        .unwrap_or(DEFAULT_RESYNC_SECS);
    Duration::from_secs(secs)
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

// ---------------------------------------------------------------------------
// GitHub source client — list the crs/ dir + fetch blobs (content-addressed).
// ---------------------------------------------------------------------------

/// One entry in a GitHub Contents directory listing.
#[derive(Debug, Deserialize)]
struct ContentEntry {
    name: String,
    #[serde(default)]
    sha: String,
    #[serde(rename = "type", default)]
    kind: String,
}

/// A GitHub git-blob response (`content` base64-encoded).
#[derive(Debug, Deserialize)]
struct BlobResponse {
    #[serde(default)]
    content: String,
    #[serde(default)]
    encoding: String,
}

/// Reads `infra/k8s/operator/crs/*.yaml` from a repo over the GitHub REST API.
///
/// Uses the API (not a `git` subprocess) deliberately: the operator image ships
/// no `git`, the token rides an `Authorization` header (no URL-embedded
/// credential to scrub), and blobs are content-addressed by SHA so a tight poll
/// only refetches files that actually changed. The API base is configurable
/// (`GITOPS_GITHUB_API`) so a GitHub-compatible host (e.g. git.hanzo.ai) works
/// with the same code.
struct GitSource {
    http: reqwest::Client,
    api_base: String,
    repo: String,
    branch: String,
    crs_path: String,
}

impl GitSource {
    fn get(&self, url: &str, token: &Token) -> reqwest::RequestBuilder {
        self.http
            .get(url)
            .header("Authorization", format!("Bearer {}", token.expose()))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// List the crs/ directory. Errors carry only the path + HTTP status — never
    /// the token (header-auth) and never a response body.
    async fn list_dir(&self, token: &Token) -> Result<Vec<ContentEntry>, String> {
        let url = format!(
            "{}/repos/{}/contents/{}?ref={}",
            self.api_base, self.repo, self.crs_path, self.branch
        );
        let resp = self
            .get(&url, token)
            .send()
            .await
            .map_err(|e| format!("list request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("list {} → HTTP {}", self.crs_path, status));
        }
        resp.json::<Vec<ContentEntry>>()
            .await
            .map_err(|e| format!("list decode failed: {e}"))
    }

    /// Fetch one blob's content by SHA (content-addressed, immutable, cacheable).
    async fn fetch_blob(&self, token: &Token, sha: &str) -> Result<String, String> {
        let url = format!("{}/repos/{}/git/blobs/{}", self.api_base, self.repo, sha);
        let resp = self
            .get(&url, token)
            .send()
            .await
            .map_err(|e| format!("blob request failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let short = &sha[..sha.len().min(12)];
            return Err(format!("blob {short} → HTTP {status}"));
        }
        let blob = resp
            .json::<BlobResponse>()
            .await
            .map_err(|e| format!("blob decode failed: {e}"))?;
        if blob.encoding != "base64" {
            return Err(format!("unexpected blob encoding: {}", blob.encoding));
        }
        decode_base64_blob(&blob.content)
    }
}

// ---------------------------------------------------------------------------
// Sweeper — one owner of the caches; runs a sweep per tick.
// ---------------------------------------------------------------------------

/// Classification of one apply relative to the live object (the drift report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Created,
    Updated,
    InSync,
}

/// Per-sweep tally — logged as the drift summary AND served at `GET /gitops`, so
/// "which CRs are not being reconciled" is a fact you can read, not a log line to
/// grep.
#[derive(Debug, Default, Clone, Serialize)]
pub struct SweepReport {
    pub created: usize,
    pub updated: usize,
    pub in_sync: usize,
    pub errors: usize,
    /// Applies the API server REFUSED with HTTP 403 — the RBAC gap. Counted apart
    /// from `errors`: a widened namespace set over an unwidened ClusterRole is
    /// otherwise indistinguishable from a transient network failure.
    pub denied: usize,
    /// Every skip — out-of-scope + unparseable + not-a-CR-file.
    pub skipped: usize,
    /// The subset of `skipped` that is a CR this loop does not own — the outage
    /// counter. `refs` names them.
    pub out_of_scope: usize,
    /// `namespace/name` of each out-of-scope CR, so the tally reads as
    /// "zen/zen is NOT being reconciled" instead of "skipped=1".
    pub refs: Vec<String>,
}

/// The last sweep's report, shared with the operator's health server. This
/// operator surfaces status over that axum server (`/healthz`, `/readyz`,
/// `POST /reconcile`) and has no metrics registry — this follows that seam rather
/// than opening a rival one.
pub type Status = Arc<Mutex<Option<SweepReport>>>;

/// Why one apply failed. `denied` (HTTP 403) is called out from every other
/// failure: widening the owned namespaces over an unwidened ClusterRole is THE
/// trap of this change, and it must be a named, loud signal.
struct Fail {
    denied: bool,
    msg: String,
}

impl Fail {
    fn other(msg: String) -> Self {
        Self { denied: false, msg }
    }
}

/// True iff the API server refused the call with HTTP 403 — the operator's
/// ClusterRole does not grant the verb on that resource in that namespace. Pure,
/// mirroring `apply::is_structural_conflict`, so the RBAC-gap signal is testable
/// without a cluster.
fn is_denied(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(resp) if resp.code == 403)
}

/// [`is_denied`] over the wrapper `apply_dynamic_as` returns.
fn is_denied_op(err: &OperatorError) -> bool {
    matches!(err, OperatorError::KubeApi(e) if is_denied(e))
}

struct Sweeper {
    client: Client,
    source: GitSource,
    scope: Scope,
    token_file: String,
    /// filename → (blob sha, content) — skip refetching unchanged files.
    blob_cache: HashMap<String, (String, String)>,
    /// GVK → resolved ApiResource (correct plural, e.g. Ingress→ingresses) via
    /// discovery. Cached so discovery runs once per Kind, not per object.
    ar_cache: HashMap<GroupVersionKind, ApiResource>,
}

impl Sweeper {
    /// Read the git token from the mounted secret file, fresh each sweep so a
    /// rotated credential is picked up without a restart (mirrors `reconcile.sh`
    /// re-reading `/creds/token`).
    async fn read_token(&self) -> Result<Token, String> {
        let raw = tokio::fs::read_to_string(&self.token_file)
            .await
            .map_err(|e| format!("read token {}: {e}", self.token_file))?;
        let t = raw.trim().to_string();
        if t.is_empty() {
            return Err(format!("token file {} is empty", self.token_file));
        }
        Ok(Token(t))
    }

    /// Resolve (and cache) the ApiResource for a GVK via discovery — the same
    /// mechanism `kubectl` uses, so irregular plurals (Ingress→ingresses,
    /// DNS→…) are always correct, for any Kind the crs/ dir declares.
    async fn resolve_ar(&mut self, gvk: &GroupVersionKind) -> Result<ApiResource, String> {
        if let Some(ar) = self.ar_cache.get(gvk) {
            return Ok(ar.clone());
        }
        let (ar, _caps) = kube::discovery::pinned_kind(&self.client, gvk)
            .await
            .map_err(|e| format!("discover {}/{} {}: {e}", gvk.group, gvk.version, gvk.kind))?;
        self.ar_cache.insert(gvk.clone(), ar.clone());
        Ok(ar)
    }

    /// Server-side apply one object; classify vs the live state for the report.
    /// The object lands in the namespace GIT declares — there is no implied
    /// namespace to fall back to, and [`Scope::decide`] already refused any object
    /// without one.
    async fn apply_one(&mut self, gvk: &GroupVersionKind, obj: &DynamicObject) -> Result<Outcome, Fail> {
        let ar = self.resolve_ar(gvk).await.map_err(Fail::other)?;
        let name = obj
            .metadata
            .name
            .clone()
            .ok_or_else(|| Fail::other("object missing metadata.name".to_string()))?;
        let ns = obj
            .metadata
            .namespace
            .clone()
            .ok_or_else(|| Fail::other(format!("object {name} missing metadata.namespace")))?;
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), &ns, &ar);

        let before = api.get_opt(&name).await.map_err(|e| Fail {
            denied: is_denied(&e),
            msg: format!("get {name}: {e}"),
        })?;
        let prev_rv = before
            .as_ref()
            .and_then(|o| o.metadata.resource_version.clone());

        let applied = apply::apply_dynamic_as(&api, obj, FIELD_MANAGER)
            .await
            .map_err(|e| Fail {
                denied: is_denied_op(&e),
                msg: format!("apply {name}: {e}"),
            })?;
        let new_rv = applied.metadata.resource_version;

        Ok(match before {
            None => Outcome::Created,
            Some(_) if prev_rv != new_rv => Outcome::Updated,
            Some(_) => Outcome::InSync,
        })
    }

    /// One reconcile sweep: list git → (SHA-cached) content → plan → apply each,
    /// classify, report. Never prunes. Returns `Err` only for a whole-sweep
    /// failure (token/list); per-object failures are counted and logged.
    async fn sweep(&mut self) -> Result<SweepReport, String> {
        let token = self.read_token().await?;
        let entries = self.source.list_dir(&token).await?;

        // Assemble the in-scope reconcilable file set, using the blob-SHA cache
        // to avoid refetching content that hasn't changed since last sweep.
        let mut files: Vec<(String, String)> = Vec::new();
        let mut live_names: HashSet<String> = HashSet::new();
        let mut refetched = 0usize;
        for e in &entries {
            if e.kind != "file" || !is_reconcilable_file(&e.name) {
                continue;
            }
            live_names.insert(e.name.clone());
            let content = match self.blob_cache.get(&e.name) {
                Some((sha, content)) if *sha == e.sha => content.clone(),
                _ => {
                    let c = self.source.fetch_blob(&token, &e.sha).await?;
                    self.blob_cache
                        .insert(e.name.clone(), (e.sha.clone(), c.clone()));
                    refetched += 1;
                    c
                }
            };
            files.push((e.name.clone(), content));
        }
        // Cache hygiene only: forget content for files no longer listed. This
        // touches NOTHING in the cluster — never-prune is about live objects.
        self.blob_cache.retain(|k, _| live_names.contains(k));

        let plan = plan(&files, &self.scope);
        let mut report = SweepReport {
            skipped: plan.skips.len(),
            ..Default::default()
        };
        for skip in &plan.skips {
            match skip {
                // Expected metadata (kustomization.yaml, README) — noise, not news.
                Skip::NotReconcilable { file } => {
                    debug!(%file, "gitops: not a CR file (left untouched)")
                }
                // A CR file git carries but nothing can parse. Never debug-only:
                // an unparseable CR is an UNRECONCILED CR.
                Skip::Unparseable { file, err } => {
                    warn!(%file, %err, "gitops: UNPARSEABLE — this CR is NOT reconciled (left untouched)")
                }
                Skip::OutOfScope {
                    file,
                    namespace,
                    name,
                    reason,
                } => {
                    report.out_of_scope += 1;
                    report.refs.push(format!("{namespace}/{name}"));
                    warn!(
                        %file, %namespace, %name, %reason,
                        "gitops: OUT OF SCOPE — this CR is NOT being reconciled (left untouched)"
                    );
                }
            }
        }
        for (gvk, obj) in &plan.applies {
            let name = obj.metadata.name.as_deref().unwrap_or_default();
            let ns = obj.metadata.namespace.as_deref().unwrap_or_default();
            match self.apply_one(gvk, obj).await {
                Ok(Outcome::Created) => {
                    report.created += 1;
                    info!(kind = %gvk.kind, namespace = %ns, %name, "gitops: CREATE");
                }
                Ok(Outcome::Updated) => {
                    report.updated += 1;
                    info!(kind = %gvk.kind, namespace = %ns, %name, "gitops: UPDATE (reverted drift)");
                }
                Ok(Outcome::InSync) => {
                    report.in_sync += 1;
                    debug!(kind = %gvk.kind, namespace = %ns, %name, "gitops: in-sync");
                }
                Err(f) if f.denied => {
                    report.denied += 1;
                    error!(
                        kind = %gvk.kind, namespace = %ns, %name, error = %f.msg,
                        "gitops: REFUSED (HTTP 403) — this loop owns the namespace but RBAC does not \
                         grant it. Widen the operator ClusterRole, or narrow GITOPS_NAMESPACES"
                    );
                }
                Err(f) => {
                    report.errors += 1;
                    warn!(kind = %gvk.kind, namespace = %ns, %name, error = %f.msg, "gitops: apply failed (skipped this tick)");
                }
            }
        }
        info!(
            source_files = files.len(),
            source_refetched = refetched,
            created = report.created,
            updated = report.updated,
            in_sync = report.in_sync,
            errors = report.errors,
            denied = report.denied,
            skipped = report.skipped,
            out_of_scope = report.out_of_scope,
            "gitops: reconcile sweep complete (never prunes)"
        );
        // The headline. A CR in git that this loop does not own is drift the loop
        // will NEVER fix, so it says so every sweep, naming names, until the set
        // is widened (or the CR leaves git). Silence here cost 28h once.
        if report.out_of_scope > 0 {
            warn!(
                count = report.out_of_scope,
                refs = %report.refs.join(","),
                owned = %namespaces_display(&self.scope.namespaces),
                "gitops: CRs in git are NOT being reconciled — their namespace is not owned. \
                 Add it to GITOPS_NAMESPACES (`*` = every namespace) to own them"
            );
        }
        if report.denied > 0 {
            error!(
                count = report.denied,
                "gitops: applies REFUSED (HTTP 403) — the operator's ClusterRole does not cover the \
                 namespaces this loop now owns. Widen it (hanzo.ai/*, secrets.lux.network/kmssecrets, \
                 core persistentvolumeclaims) or narrow GITOPS_NAMESPACES"
            );
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Config + entrypoint.
// ---------------------------------------------------------------------------

/// Resolved runtime config, from the environment (all defaulted).
#[derive(Clone, Debug)]
struct GitopsConfig {
    repo: String,
    branch: String,
    crs_path: String,
    api_base: String,
    token_file: String,
    scope: Scope,
    resync: Duration,
}

impl GitopsConfig {
    fn from_env() -> Self {
        let namespaces = resolve_namespaces(
            std::env::var("GITOPS_NAMESPACES").ok().as_deref(),
            std::env::var("GITOPS_NAMESPACE").ok().as_deref(),
        );
        let scope = Scope::new(
            namespaces,
            &std::env::var("GITOPS_APPLY_SCOPE").unwrap_or_default(),
        );
        Self {
            repo: env_or("GITOPS_REPO", DEFAULT_REPO),
            branch: env_or("GITOPS_BRANCH", DEFAULT_BRANCH),
            crs_path: env_or("GITOPS_CRS_PATH", DEFAULT_CRS_PATH),
            api_base: env_or("GITOPS_GITHUB_API", DEFAULT_API_BASE),
            token_file: env_or("GITOPS_TOKEN_FILE", DEFAULT_TOKEN_FILE),
            scope,
            resync: parse_resync(std::env::var("GITOPS_RESYNC_SECS").ok().as_deref()),
        }
    }
}

/// Opt-in entrypoint — slots into `run_all_controllers`' `tokio::join!` with the
/// `(client, notify, enabled)` shape. When `enabled` is false (default —
/// `GITOPS_RECONCILE_ENABLED` unset) it returns immediately and nothing is ever
/// read from git or applied, so the existing CR→workload controllers are
/// completely undisturbed.
///
/// `reconcile_now` is the shared handle the `POST /reconcile` webhook notifies:
/// each loop iteration wakes on whichever comes first — the poll tick or a push
/// notification — so a git push reconciles instantly while the tight poll stays
/// the guaranteed fallback.
pub async fn run_gitops_controller(
    client: Client,
    reconcile_now: Arc<Notify>,
    enabled: bool,
    status: Status,
) {
    if !enabled {
        info!("Gitops reconcile disabled (set GITOPS_RECONCILE_ENABLED=true to enable)");
        return;
    }
    let config = GitopsConfig::from_env();
    info!(
        repo = %config.repo,
        branch = %config.branch,
        crs_path = %config.crs_path,
        namespaces = %namespaces_display(&config.scope.namespaces),
        scoped_names = config.scope.names.as_ref().map(|s| s.len()).unwrap_or(0),
        resync_secs = config.resync.as_secs(),
        "Starting native gitops reconcile (git → CR apply loop; replaces the gitops-reconcile CronJob)"
    );
    if config.scope.namespaces.is_empty() {
        warn!(
            "gitops scope: EVERY namespace (GITOPS_NAMESPACES=*). This loop applies any CR the git \
             path declares, wherever it declares it — bounded only by the operator's ClusterRole."
        );
    }
    if config.scope.names.is_none() {
        info!(
            namespaces = %namespaces_display(&config.scope.namespaces),
            "gitops scope: whole namespace (ownership-based default). Set GITOPS_APPLY_SCOPE to \
             start with a vetted subset."
        );
    }

    let http = match reqwest::Client::builder()
        .user_agent("hanzo-operator-gitops")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "gitops: failed to build HTTP client; loop not started");
            return;
        }
    };
    let source = GitSource {
        http,
        api_base: config.api_base,
        repo: config.repo,
        branch: config.branch,
        crs_path: config.crs_path,
    };
    let mut sweeper = Sweeper {
        client,
        source,
        scope: config.scope,
        token_file: config.token_file,
        blob_cache: HashMap::new(),
        ar_cache: HashMap::new(),
    };

    let mut tick = tokio::time::interval(config.resync);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = reconcile_now.notified() => {
                info!("gitops: webhook-triggered reconcile");
            }
        }
        // A sweep failure (token/list/network) is transient — log and wait for
        // the next tick. NEVER crash the operator or block the CR controllers.
        match sweeper.sweep().await {
            // Publish for `GET /gitops`. A poisoned lock is not worth panicking
            // over in a loop whose contract is "never panics" — the next sweep
            // re-publishes.
            Ok(report) => {
                if let Ok(mut slot) = status.lock() {
                    *slot = Some(report);
                }
            }
            Err(e) => warn!(error = %e, "gitops: reconcile sweep failed; will retry next tick"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — pure bits only (no cluster / no network).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn service_yaml(name: &str, ns: &str) -> String {
        format!(
            "apiVersion: hanzo.ai/v1\nkind: Service\nmetadata:\n  name: {name}\n  namespace: {ns}\nspec:\n  replicas: 1\n"
        )
    }

    fn obj(yaml: &str) -> DynamicObject {
        parse_manifests(yaml).unwrap().into_iter().next().unwrap()
    }

    // ---- file selection ----

    #[test]
    fn reconcilable_only_yaml_never_kustomization() {
        assert!(is_reconcilable_file("analytics.yaml"));
        assert!(is_reconcilable_file("hanzo-app.yaml"));
        assert!(!is_reconcilable_file("kustomization.yaml"));
        assert!(!is_reconcilable_file("README.md"));
        assert!(!is_reconcilable_file("values.yml")); // crs/ uses .yaml
        assert!(!is_reconcilable_file("notes.txt"));
    }

    // ---- kind-agnostic parse / GVK ----

    #[test]
    fn gvk_of_reads_group_version_kind() {
        let s = obj(&service_yaml("analytics", "hanzo"));
        let g = gvk_of(&s).unwrap();
        assert_eq!(g.group, "hanzo.ai");
        assert_eq!(g.version, "v1");
        assert_eq!(g.kind, "Service");
    }

    #[test]
    fn gvk_of_core_group_is_empty() {
        let pvc = obj("apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: chat-db\n  namespace: hanzo\n");
        let g = gvk_of(&pvc).unwrap();
        assert_eq!(g.group, "");
        assert_eq!(g.version, "v1");
        assert_eq!(g.kind, "PersistentVolumeClaim");
    }

    #[test]
    fn parse_manifests_is_kind_agnostic() {
        // Service (CRD), KMSSecret (different group), PVC (core), App (future).
        for (yaml, group, kind) in [
            (service_yaml("x", "hanzo"), "hanzo.ai", "Service"),
            (
                "apiVersion: secrets.lux.network/v1alpha1\nkind: KMSSecret\nmetadata:\n  name: k\n  namespace: hanzo\n".to_string(),
                "secrets.lux.network",
                "KMSSecret",
            ),
            (
                "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata:\n  name: p\n  namespace: hanzo\n".to_string(),
                "",
                "PersistentVolumeClaim",
            ),
            (
                "apiVersion: hanzo.ai/v1\nkind: App\nmetadata:\n  name: a\n  namespace: hanzo\n".to_string(),
                "hanzo.ai",
                "App",
            ),
        ] {
            let g = gvk_of(&obj(&yaml)).unwrap();
            assert_eq!(g.group, group, "group for {kind}");
            assert_eq!(g.kind, kind);
        }
    }

    #[test]
    fn parse_manifests_multidoc_and_skips_empty() {
        let multi = format!(
            "{}\n---\n\n---\n{}",
            service_yaml("a", "hanzo"),
            service_yaml("b", "hanzo")
        );
        let objs = parse_manifests(&multi).unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].metadata.name.as_deref(), Some("a"));
        assert_eq!(objs[1].metadata.name.as_deref(), Some("b"));
    }

    #[test]
    fn parse_manifests_skips_typeless_document() {
        // A doc with no apiVersion/kind is not an object → skipped, not an error.
        let objs = parse_manifests("just: data\nwithout: a-kind\n").unwrap();
        assert!(objs.is_empty());
    }

    // ---- namespace-set resolution (the env contract + its precedence) ----

    fn scope(nss: &[&str], raw_scope: &str) -> Scope {
        Scope::new(nss.iter().map(|s| s.to_string()).collect(), raw_scope)
    }

    /// (a) THE NO-OP GUARANTEE. Absent config ⇒ `{hanzo}` — today's behavior,
    /// unchanged — so deploying this binary widens nothing until someone says so.
    #[test]
    fn absent_config_owns_only_hanzo() {
        assert_eq!(
            resolve_namespaces(None, None),
            HashSet::from(["hanzo".to_string()])
        );
        let s = Scope::new(resolve_namespaces(None, None), "");
        assert!(s.decide(&obj(&service_yaml("cloud", "hanzo"))).is_ok());
        // The zen CR — still out of scope until deliberately widened.
        assert_eq!(
            s.decide(&obj(&service_yaml("zen", "zen"))),
            Err(Reason::Namespace)
        );
    }

    /// A blank / separators-only value is an ACCIDENT (`value: ""` in a manifest),
    /// never an intent to own the cluster. It must fall through to the default,
    /// not to the fail-open empty set.
    #[test]
    fn blank_namespaces_falls_back_and_never_means_all() {
        for raw in ["", "   ", "\t", ",", " , , "] {
            assert_eq!(
                resolve_namespaces(Some(raw), None),
                HashSet::from(["hanzo".to_string()]),
                "blank GITOPS_NAMESPACES {raw:?} must not widen"
            );
        }
    }

    /// Precedence: plural beats singular beats default. The singular
    /// `GITOPS_NAMESPACE` keeps working exactly as before.
    #[test]
    fn namespaces_precedence_plural_then_singular_then_default() {
        assert_eq!(
            resolve_namespaces(Some("a,b"), Some("ignored")),
            HashSet::from(["a".to_string(), "b".to_string()])
        );
        assert_eq!(
            resolve_namespaces(None, Some("legacy")),
            HashSet::from(["legacy".to_string()])
        );
        assert_eq!(
            resolve_namespaces(Some("  "), Some("legacy")),
            HashSet::from(["legacy".to_string()])
        );
    }

    /// (c) `*` ⇒ the EMPTY set ⇒ every namespace in scope — the documented
    /// "widens to empty" end-state, reachable only by saying so explicitly.
    #[test]
    fn star_means_every_namespace() {
        assert!(resolve_namespaces(Some(ALL), None).is_empty());
        assert!(resolve_namespaces(Some("hanzo, *"), None).is_empty());
        let s = Scope::new(resolve_namespaces(Some(ALL), None), "");
        for ns in ["hanzo", "zen", "kube-system", "anything"] {
            assert!(
                s.decide(&obj(&service_yaml("x", ns))).is_ok(),
                "{ns} must be in scope under {ALL}"
            );
        }
    }

    // ---- scope predicate ----

    /// (b) An explicit set owns EXACTLY those namespaces — the zen fix. Anything
    /// else is refused WITH a reason (the caller logs it; never silence).
    #[test]
    fn explicit_set_owns_exactly_those_namespaces() {
        let s = scope(&["hanzo", "zen"], "");
        assert!(s.decide(&obj(&service_yaml("cloud", "hanzo"))).is_ok());
        assert!(s.decide(&obj(&service_yaml("zen", "zen"))).is_ok());
        assert_eq!(
            s.decide(&obj(&service_yaml("evil", "kube-system"))),
            Err(Reason::Namespace)
        );
    }

    #[test]
    fn scope_vetted_subset_narrows() {
        let s = scope(&["hanzo"], "analytics, billing world");
        assert!(s.names.as_ref().unwrap().len() == 3);
        assert!(s.decide(&obj(&service_yaml("analytics", "hanzo"))).is_ok());
        assert!(s.decide(&obj(&service_yaml("world", "hanzo"))).is_ok());
        // Present in the namespace but NOT in the vetted subset → skipped.
        assert_eq!(
            s.decide(&obj(&service_yaml("chat", "hanzo"))),
            Err(Reason::Name)
        );
        // In the subset but a namespace we do not own → refused on the NAMESPACE
        // gate: it runs first, so a bare name never widens past the owned set.
        assert_eq!(
            s.decide(&obj(&service_yaml("analytics", "other"))),
            Err(Reason::Namespace)
        );
    }

    /// The vetted subset composes with a MULTI-namespace set: a bare name is
    /// vetted in any owned namespace, and a pinned `ns/name` in exactly one.
    #[test]
    fn vetted_subset_composes_with_the_namespace_set() {
        let bare = scope(&["hanzo", "zen"], "cloud");
        assert!(bare.decide(&obj(&service_yaml("cloud", "hanzo"))).is_ok());
        assert!(bare.decide(&obj(&service_yaml("cloud", "zen"))).is_ok());

        let pinned = scope(&["hanzo", "zen"], "zen/cloud");
        assert!(pinned.decide(&obj(&service_yaml("cloud", "zen"))).is_ok());
        // Same NAME, owned namespace, but not the pinned pair → refused.
        assert_eq!(
            pinned.decide(&obj(&service_yaml("cloud", "hanzo"))),
            Err(Reason::Name)
        );
    }

    /// A CR ref is (namespace, name) — a malformed entry is dropped rather than
    /// silently widening or narrowing on a half-parsed key.
    #[test]
    fn ref_parse_rejects_malformed_entries() {
        assert_eq!(
            Ref::parse("cloud"),
            Some(Ref {
                namespace: None,
                name: "cloud".into()
            })
        );
        assert_eq!(
            Ref::parse("zen/cloud"),
            Some(Ref {
                namespace: Some("zen".into()),
                name: "cloud".into()
            })
        );
        for bad in ["/cloud", "zen/", "/", "a/b/c", "  "] {
            assert_eq!(Ref::parse(bad), None, "{bad:?} must not parse");
        }
    }

    /// A namespace-less (or cluster-scoped) object is out of scope in EVERY
    /// configuration — including `*`, where the namespace gate admits everything.
    /// There is no implied namespace to apply it into.
    #[test]
    fn namespaceless_object_is_refused_even_under_star() {
        let cluster_scoped = obj("apiVersion: hanzo.ai/v1\nkind: Service\nmetadata:\n  name: x\n");
        for s in [
            scope(&["hanzo"], ""),
            Scope::new(resolve_namespaces(Some(ALL), None), ""),
        ] {
            assert_eq!(s.decide(&cluster_scoped), Err(Reason::Namespace));
        }
    }

    // ---- plan(): never-prune + drift skips ----

    /// (d) NAME COLLISION. With more than one owned namespace the same name can
    /// exist twice. The two CRs are DISTINCT objects: both plan, both keep their
    /// own namespace, neither clobbers the other. A bare-name key anywhere in the
    /// pipeline would collapse them into one.
    #[test]
    fn same_name_in_two_namespaces_is_two_distinct_objects() {
        let files = vec![
            ("hanzo-cloud.yaml".into(), service_yaml("cloud", "hanzo")),
            ("zen-cloud.yaml".into(), service_yaml("cloud", "zen")),
        ];
        let p = plan(&files, &scope(&["hanzo", "zen"], ""));
        assert_eq!(p.applies.len(), 2, "both must survive planning");
        let mut keys: Vec<(String, String)> = p
            .applies
            .iter()
            .map(|(_, o)| {
                (
                    o.metadata.namespace.clone().unwrap(),
                    o.metadata.name.clone().unwrap(),
                )
            })
            .collect();
        keys.sort();
        // Identity is the PAIR — same name, different namespace, two objects.
        assert_eq!(
            keys,
            vec![
                ("hanzo".to_string(), "cloud".to_string()),
                ("zen".to_string(), "cloud".to_string())
            ]
        );
        // And the apply target is each object's OWN namespace: no implied
        // fallback that would send zen/cloud into hanzo (the clobber).
        for (_, o) in &p.applies {
            assert!(o.metadata.namespace.is_some());
        }
    }

    /// The name gate keys on the PAIR too: pinning `zen/cloud` must not vet
    /// `hanzo/cloud`, and vice versa.
    #[test]
    fn name_gate_keys_on_the_pair_not_the_bare_name() {
        let files = vec![
            ("hanzo-cloud.yaml".into(), service_yaml("cloud", "hanzo")),
            ("zen-cloud.yaml".into(), service_yaml("cloud", "zen")),
        ];
        let p = plan(&files, &scope(&["hanzo", "zen"], "zen/cloud"));
        assert_eq!(p.applies.len(), 1);
        assert_eq!(p.applies[0].1.metadata.namespace.as_deref(), Some("zen"));
        // The other is skipped — and named, with a reason.
        assert!(p.skips.iter().any(|s| matches!(
            s,
            Skip::OutOfScope { namespace, name, reason: Reason::Name, .. }
                if namespace == "hanzo" && name == "cloud"
        )));
    }

    /// (e) THE OUTAGE. A CR whose namespace we do not own must be counted AND
    /// carry its (namespace, name) + reason — the drift report's whole job. This
    /// is the zen CR: `crs/zen.yaml`, `namespace: zen`, skipped behind a bare
    /// `skipped=1` for 28h.
    #[test]
    fn out_of_scope_skip_names_the_cr_and_is_never_silent() {
        let files = vec![("zen.yaml".into(), service_yaml("zen", "zen"))];
        let p = plan(&files, &scope(&["hanzo"], ""));
        assert!(p.applies.is_empty());
        assert_eq!(p.skips.len(), 1);
        // Not a bare count: the skip carries everything an operator needs to see
        // WHICH CR is unreconciled and WHY, without a cluster or a debug log.
        match &p.skips[0] {
            Skip::OutOfScope {
                file,
                namespace,
                name,
                reason,
            } => {
                assert_eq!(file, "zen.yaml");
                assert_eq!(namespace, "zen");
                assert_eq!(name, "zen");
                assert_eq!(*reason, Reason::Namespace);
                // The reason renders as an actionable sentence, not an enum name.
                assert!(reason.to_string().contains("GITOPS_NAMESPACES"));
            }
            other => panic!("expected OutOfScope, got {other:?}"),
        }
        // Widening the set reconciles it — the fix, end to end.
        let widened = plan(&files, &scope(&["hanzo", "zen"], ""));
        assert_eq!(widened.applies.len(), 1);
        assert!(widened.skips.is_empty());
    }

    /// A 403 is the RBAC gap and must be distinguishable from a transient error —
    /// widening the owned set over an unwidened ClusterRole is the trap.
    #[test]
    fn denial_is_classified_apart_from_other_failures() {
        let forbidden = kube::Error::Api(Box::new(kube::core::Status {
            code: 403,
            message: "services.hanzo.ai is forbidden".into(),
            reason: "Forbidden".into(),
            ..Default::default()
        }));
        assert!(is_denied(&forbidden));
        assert!(is_denied_op(&OperatorError::KubeApi(forbidden)));
        for code in [404, 409, 422, 500] {
            assert!(!is_denied(&kube::Error::Api(Box::new(kube::core::Status {
                code,
                ..Default::default()
            }))));
        }
    }

    #[test]
    fn namespaces_display_says_all_for_the_empty_set() {
        assert!(namespaces_display(&HashSet::new()).contains("every namespace"));
        // Sorted → the log line is stable across runs (HashSet order is not).
        assert_eq!(
            namespaces_display(&HashSet::from(["zen".to_string(), "hanzo".to_string()])),
            "hanzo,zen"
        );
    }

    #[test]
    fn plan_applies_in_scope_and_reports_skips() {
        let files = vec![
            ("analytics.yaml".into(), service_yaml("analytics", "hanzo")),
            ("world.yaml".into(), service_yaml("world", "hanzo")),
            // wrong namespace → OutOfScope skip
            ("evil.yaml".into(), service_yaml("evil", "kube-system")),
            // not a CR file → NotReconcilable skip
            ("kustomization.yaml".into(), "resources: []\n".into()),
            // bad yaml → Unparseable skip
            (
                "broken.yaml".into(),
                "apiVersion: hanzo.ai/v1\nkind: Service\n  bad: [indent\n".into(),
            ),
        ];
        let scope = scope(&["hanzo"], "");
        let p = plan(&files, &scope);
        assert_eq!(p.applies.len(), 2, "only the two in-scope services apply");
        let names: Vec<_> = p
            .applies
            .iter()
            .map(|(_, o)| o.metadata.name.clone().unwrap())
            .collect();
        assert!(names.contains(&"analytics".to_string()));
        assert!(names.contains(&"world".to_string()));
        assert!(p
            .skips
            .iter()
            .any(|s| matches!(s, Skip::OutOfScope { namespace, name, .. }
                if namespace == "kube-system" && name == "evil")));
        assert!(p
            .skips
            .iter()
            .any(|s| matches!(s, Skip::NotReconcilable { file } if file == "kustomization.yaml")));
        assert!(p
            .skips
            .iter()
            .any(|s| matches!(s, Skip::Unparseable { file, .. } if file == "broken.yaml")));
    }

    #[test]
    fn plan_never_prunes_a_removed_file() {
        // A file present in one sweep and absent the next must NEVER produce a
        // delete — the Plan type has no delete variant, so a removed CR simply
        // vanishes from `applies` and is left alone in the cluster.
        let scope = scope(&["hanzo"], "");
        let with_both = vec![
            ("a.yaml".into(), service_yaml("a", "hanzo")),
            ("b.yaml".into(), service_yaml("b", "hanzo")),
        ];
        let with_one = vec![("a.yaml".into(), service_yaml("a", "hanzo"))];

        let p_both = plan(&with_both, &scope);
        let p_one = plan(&with_one, &scope);
        assert_eq!(p_both.applies.len(), 2);
        assert_eq!(p_one.applies.len(), 1);
        // The removed object contributes ZERO plan entries — not an apply, and
        // (crucially) not any kind of delete/skip either.
        assert!(p_one
            .applies
            .iter()
            .all(|(_, o)| o.metadata.name.as_deref() != Some("b")));
        assert!(
            p_one.skips.is_empty(),
            "a removed file yields no action at all"
        );
    }

    // ---- base64 blob decode ----

    #[test]
    fn base64_blob_tolerates_github_newlines() {
        // GitHub wraps content at 60 chars with \n. "hello world" → base64 with
        // an injected newline must still decode.
        let wrapped = "aGVsbG8g\nd29ybGQ=\n";
        assert_eq!(decode_base64_blob(wrapped).unwrap(), "hello world");
    }

    #[test]
    fn base64_blob_rejects_garbage() {
        assert!(decode_base64_blob("!!!not-base64!!!").is_err());
    }

    // ---- resync parse ----

    #[test]
    fn resync_defaults_and_floors() {
        assert_eq!(parse_resync(None).as_secs(), DEFAULT_RESYNC_SECS);
        assert_eq!(parse_resync(Some("garbage")).as_secs(), DEFAULT_RESYNC_SECS);
        assert_eq!(parse_resync(Some("30")).as_secs(), 30);
        assert_eq!(parse_resync(Some("120")).as_secs(), 120);
        // Below the floor is clamped up — no hammering the git host.
        assert_eq!(parse_resync(Some("1")).as_secs(), MIN_RESYNC_SECS);
    }

    // ---- token never logged ----

    #[test]
    fn token_debug_is_redacted() {
        let t = Token("ghp_super_secret_value".into());
        assert_eq!(format!("{t:?}"), "Token(***)");
        assert!(!format!("{t:?}").contains("secret"));
        // And it survives to the one place that needs it.
        assert_eq!(t.expose(), "ghp_super_secret_value");
    }
}
