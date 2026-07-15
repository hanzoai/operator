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
//! * **Ownership is the git path, not a hand-maintained allow-list** — every CR
//!   declared under `crs_path` is ours, in whatever namespace it declares; no
//!   string list to maintain. RBAC bounds what the operator may touch (it holds a
//!   ClusterRole), so the loop does not restate that boundary a second time.
//!   `GITOPS_NAMESPACE` and `GITOPS_APPLY_SCOPE` optionally narrow by namespace /
//!   by name for a cautious rollout, then widen to empty.
//!
//! ## Fail-safe by construction
//!
//! Opt-in (`GITOPS_RECONCILE_ENABLED=true`, default off) and additive — it runs
//! ALONGSIDE the CR→workload controllers and never blocks them. A source failure
//! (clone/list error, bad YAML, missing token) is logged and retried next tick;
//! it never crashes the operator (the loop never `?`-propagates out of its body,
//! never `unwrap`s, never panics).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use kube::api::Api;
use kube::core::{ApiResource, DynamicObject, GroupVersionKind, TypeMeta};
use kube::Client;
use serde::Deserialize;
use tokio::sync::Notify;
use tracing::{debug, error, info, warn};

use crate::apply;

/// SSA field manager for git-sourced applies — distinct from the CR
/// controllers' `hanzo-operator` so a git-driven apply is attributable and never
/// silently fights a per-Kind reconcile over the same field.
const FIELD_MANAGER: &str = "hanzo-operator-gitops";

const DEFAULT_REPO: &str = "hanzoai/universe";
const DEFAULT_BRANCH: &str = "main";
const DEFAULT_CRS_PATH: &str = "infra/k8s/operator/crs";
const DEFAULT_API_BASE: &str = "https://api.github.com";
const DEFAULT_TOKEN_FILE: &str = "/creds/token";

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

/// A narrowing set over one axis of the scope. `None` admits every value — the
/// default; `Some(set)` admits only its members. Namespace and name are the same
/// concept (which declared CRs may we apply), so they are the same type, parsed
/// by one function and read by one predicate.
type Filter = Option<HashSet<String>>;

/// Parse a comma/whitespace-separated env value into a [`Filter`]. Empty → `None`.
fn filter(raw: &str) -> Filter {
    let set: HashSet<String> = raw
        .split([',', ' ', '\t', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if set.is_empty() {
        None
    } else {
        Some(set)
    }
}

/// Whether `f` admits `v`. An absent filter admits everything.
fn admits(f: &Filter, v: &str) -> bool {
    match f {
        None => true,
        Some(set) => set.contains(v),
    }
}

/// Which CRs this loop may apply. A CR is in-scope iff every axis admits it.
///
/// Ownership is the GIT PATH: a CR declared under `crs_path` is ours, in whatever
/// namespace it declares. The namespace is data on the CR — it says where the CR
/// lives, not whether we may apply it. What the operator may touch is decided by
/// RBAC (it holds a ClusterRole), so an app-level namespace allow-list would only
/// restate that boundary in a second place, and a second place is where the two
/// drift apart: a CR correct in git sat un-reconciled because it declared a
/// namespace this filter did not name, and drift is silent — the app keeps
/// serving its old image while git says otherwise.
///
/// Both axes default to `None` (own everything declared). `GITOPS_NAMESPACE` and
/// `GITOPS_APPLY_SCOPE` narrow by namespace / by name for a cautious rollout.
#[derive(Clone, Debug, Default)]
struct Scope {
    namespaces: Filter,
    names: Filter,
}

impl Scope {
    /// Build from the raw `GITOPS_NAMESPACE` + `GITOPS_APPLY_SCOPE` values.
    /// Empty → `None` on that axis.
    fn new(raw_namespaces: &str, raw_names: &str) -> Self {
        Self {
            namespaces: filter(raw_namespaces),
            names: filter(raw_names),
        }
    }

    /// In-scope predicate over a parsed object. Pure — the load-bearing safety
    /// gate, unit-tested without a cluster. An object that declares no namespace
    /// or no name states no place and no identity, so no scope admits it: it is
    /// left alone rather than applied somewhere implied.
    fn allows(&self, obj: &DynamicObject) -> bool {
        let (Some(ns), Some(name)) = (
            obj.metadata.namespace.as_deref(),
            obj.metadata.name.as_deref(),
        ) else {
            return false;
        };
        admits(&self.namespaces, ns) && admits(&self.names, name)
    }
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
    NotReconcilable { file: String },
    Unparseable { file: String, err: String },
    OutOfScope { file: String, name: String },
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
            if !scope.allows(&obj) {
                plan.skips.push(Skip::OutOfScope {
                    file: name.clone(),
                    name: obj.metadata.name.clone().unwrap_or_default(),
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

/// Per-sweep tally, logged as the drift summary.
#[derive(Debug, Default)]
struct SweepReport {
    created: usize,
    updated: usize,
    in_sync: usize,
    errors: usize,
    skipped: usize,
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
    async fn apply_one(
        &mut self,
        gvk: &GroupVersionKind,
        obj: &DynamicObject,
    ) -> Result<Outcome, String> {
        let ar = self.resolve_ar(gvk).await?;
        let name = obj
            .metadata
            .name
            .clone()
            .ok_or_else(|| "object missing metadata.name".to_string())?;
        // The CR declares where it lives; there is no implied namespace to fall
        // back to. `Scope::allows` already refused any object without one.
        let ns = obj
            .metadata
            .namespace
            .clone()
            .ok_or_else(|| format!("object {name} missing metadata.namespace"))?;
        let api: Api<DynamicObject> = Api::namespaced_with(self.client.clone(), &ns, &ar);

        let before = api
            .get_opt(&name)
            .await
            .map_err(|e| format!("get {name}: {e}"))?;
        let prev_rv = before
            .as_ref()
            .and_then(|o| o.metadata.resource_version.clone());

        let applied = apply::apply_dynamic_as(&api, obj, FIELD_MANAGER)
            .await
            .map_err(|e| format!("apply {name}: {e}"))?;
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
            debug!(?skip, "gitops: skipped (left untouched)");
        }
        for (gvk, obj) in &plan.applies {
            let name = obj.metadata.name.as_deref().unwrap_or_default();
            match self.apply_one(gvk, obj).await {
                Ok(Outcome::Created) => {
                    report.created += 1;
                    info!(kind = %gvk.kind, %name, "gitops: CREATE");
                }
                Ok(Outcome::Updated) => {
                    report.updated += 1;
                    info!(kind = %gvk.kind, %name, "gitops: UPDATE (reverted drift)");
                }
                Ok(Outcome::InSync) => {
                    report.in_sync += 1;
                    debug!(kind = %gvk.kind, %name, "gitops: in-sync");
                }
                Err(e) => {
                    report.errors += 1;
                    warn!(kind = %gvk.kind, %name, error = %e, "gitops: apply failed (skipped this tick)");
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
            skipped = report.skipped,
            "gitops: reconcile sweep complete (never prunes)"
        );
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
        let scope = Scope::new(
            &std::env::var("GITOPS_NAMESPACE").unwrap_or_default(),
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
pub async fn run_gitops_controller(client: Client, reconcile_now: Arc<Notify>, enabled: bool) {
    if !enabled {
        info!("Gitops reconcile disabled (set GITOPS_RECONCILE_ENABLED=true to enable)");
        return;
    }
    let config = GitopsConfig::from_env();
    info!(
        repo = %config.repo,
        branch = %config.branch,
        crs_path = %config.crs_path,
        scoped_namespaces = config.scope.namespaces.as_ref().map(|s| s.len()).unwrap_or(0),
        scoped_names = config.scope.names.as_ref().map(|s| s.len()).unwrap_or(0),
        resync_secs = config.resync.as_secs(),
        "Starting native gitops reconcile (git → CR apply loop; replaces the gitops-reconcile CronJob)"
    );
    if config.scope.namespaces.is_none() && config.scope.names.is_none() {
        info!(
            "gitops scope: every CR declared under the git path, in whatever namespace it declares \
             (ownership is the path; RBAC bounds what the operator may touch). Set \
             GITOPS_NAMESPACE / GITOPS_APPLY_SCOPE to narrow by namespace / by name."
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
        if let Err(e) = sweeper.sweep().await {
            warn!(error = %e, "gitops: reconcile sweep failed; will retry next tick");
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

    // ---- scope predicate ----

    /// The default owns every CR the git path declares, in whatever namespace it
    /// declares — the regression test for a real outage: an App CR correct in git
    /// declared `namespace: zen`, the scope named only `hanzo`, so the loop
    /// skipped it and the app served a stale image for over a day while git said
    /// otherwise. Drift is silent, which is what made it expensive.
    #[test]
    fn scope_default_owns_every_declared_namespace() {
        let scope = Scope::new("", "");
        assert!(scope.namespaces.is_none());
        assert!(scope.names.is_none());
        assert!(scope.allows(&obj(&service_yaml("analytics", "hanzo"))));
        // The CR that was skipped. Its namespace is data, not a permission.
        assert!(scope.allows(&obj(&service_yaml("zen", "zen"))));
        assert!(scope.allows(&obj(&service_yaml("anything", "any-namespace"))));
    }

    /// `GITOPS_NAMESPACE` narrows for a cautious rollout — the axis still exists,
    /// it just is not the default and is not the ownership boundary.
    #[test]
    fn scope_namespace_filter_narrows() {
        let scope = Scope::new("hanzo, zen", "");
        assert!(scope.allows(&obj(&service_yaml("analytics", "hanzo"))));
        assert!(scope.allows(&obj(&service_yaml("zen", "zen"))));
        assert!(!scope.allows(&obj(&service_yaml("evil", "kube-system"))));
    }

    #[test]
    fn scope_vetted_subset_narrows() {
        let scope = Scope::new("hanzo", "analytics, billing world");
        assert!(scope.names.as_ref().unwrap().len() == 3);
        assert!(scope.allows(&obj(&service_yaml("analytics", "hanzo"))));
        assert!(scope.allows(&obj(&service_yaml("world", "hanzo"))));
        // Present in the namespace but NOT in the vetted subset → skipped.
        assert!(!scope.allows(&obj(&service_yaml("chat", "hanzo"))));
        // In the subset but wrong namespace → still rejected.
        assert!(!scope.allows(&obj(&service_yaml("analytics", "other"))));
    }

    #[test]
    fn scope_rejects_namespaceless_object() {
        let scope = Scope::new("hanzo", "");
        let cluster_scoped = obj("apiVersion: hanzo.ai/v1\nkind: Service\nmetadata:\n  name: x\n");
        assert!(!scope.allows(&cluster_scoped));
    }

    // ---- plan(): never-prune + drift skips ----

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
        let scope = Scope::new("hanzo", "");
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
            .any(|s| matches!(s, Skip::OutOfScope { name, .. } if name == "evil")));
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
        let scope = Scope::new("hanzo", "");
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
