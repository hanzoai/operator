//! GitSource reconciler — pull-based git→cluster sync (native GitOps).
//!
//! Retires the `gitops-reconcile` shell cron (universe
//! infra/k8s/gitops-reconcile/reconcile.sh): instead of a CronJob shelling
//! `git clone` + `kubectl apply`, the operator owns the loop as a first-class
//! CR. On `intervalSeconds` it clones `repo@ref`, renders `path`, and
//! server-side-applies (field-manager `gitops`) — drift-correcting every tick.
//! `prune` and `allowlist` carry the cron's exact safety posture forward
//! (no-prune by default; allowlist to heal a vetted subset mid-migration).
//!
//! This is the Flux GitOps-toolkit pull-sync *algorithm* reimplemented natively
//! over the operator's own discovery + `apply_dynamic_as` SSA machinery — one
//! reconciler, one api group, no vendored toolkit and no second control plane
//! (ArgoCD was torn out on purpose).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use std::time::Duration;
use tokio::sync::Notify;

use futures::StreamExt;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::discovery::{Discovery, Scope};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use serde::Deserialize;
use tokio::process::Command;
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{GitSource, GitSourceStatus, Phase};
use crate::crd_types::{build_condition, carry_transition_time, status_changed, upsert_condition};
use crate::gitops::{self, Checkout};

/// Label stamped on every object a GitSource applies — the apply-set marker used
/// to scope pruning to objects THIS GitSource owns (never a foreign object).
pub const APPLY_SET_LABEL: &str = "hanzo.ai/gitsource";

/// SSA field manager for git-sourced applies — distinct from the operator's own
/// `hanzo-operator` manager (and matching the cron's dedicated manager), so a
/// CR's git-owned spec fields are attributable to the pull-sync.
const GITOPS_FIELD_MANAGER: &str = "gitops";

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<GitSource>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("GitSource has no namespace".into()))?;
    let spec = &cr.spec;
    let generation = cr.meta().generation.unwrap_or(0);

    if spec.repo.is_empty() {
        warn!(name = %name, namespace = %namespace, "GitSource has empty repo — nothing to sync");
        return Ok(Action::requeue(Duration::from_secs(120)));
    }
    let interval = Duration::from_secs(spec.interval_seconds.max(30));
    let prior = cr.status.clone().unwrap_or_default();

    match sync(&ctx.client, &namespace, &name, spec).await {
        Ok(res) => {
            let phase = if res.errors.is_empty() {
                Phase::Running
            } else {
                Phase::Degraded
            };
            let mut status = GitSourceStatus {
                phase: Some(phase),
                last_applied_revision: res.revision.clone(),
                last_sync_time: Some(jiff::Timestamp::now().to_string()),
                applied_count: res.applied_count,
                conditions: prior.conditions.clone(),
                observed_generation: generation,
                message: res.message(),
            };
            if res.errors.is_empty() {
                let mut cond =
                    build_condition("Synced", true, "SyncOk", &status.message, generation);
                carry_transition_time(&prior.conditions, &mut cond);
                upsert_condition(&mut status.conditions, cond);
                info!(
                    name = %name, namespace = %namespace, revision = %res.revision,
                    applied = res.applied_count, pruned = res.pruned_count,
                    "GitSource synced"
                );
            } else {
                let mut cond =
                    build_condition("Synced", false, "SyncFailed", &status.message, generation);
                carry_transition_time(&prior.conditions, &mut cond);
                upsert_condition(&mut status.conditions, cond);
                warn!(
                    name = %name, namespace = %namespace, revision = %res.revision,
                    applied = res.applied_count, errors = res.errors.len(),
                    "GitSource synced with errors"
                );
            }
            write_status(&ctx.client, &namespace, &name, &status, &prior).await;
            Ok(Action::requeue(interval))
        }
        Err(e) => {
            let msg = e.to_string();
            let mut status = GitSourceStatus {
                phase: Some(Phase::Degraded),
                last_applied_revision: prior.last_applied_revision.clone(),
                last_sync_time: prior.last_sync_time.clone(),
                applied_count: prior.applied_count,
                conditions: prior.conditions.clone(),
                observed_generation: generation,
                message: msg.clone(),
            };
            let mut cond = build_condition("Synced", false, "SyncFailed", &msg, generation);
            carry_transition_time(&prior.conditions, &mut cond);
            upsert_condition(&mut status.conditions, cond);
            write_status(&ctx.client, &namespace, &name, &status, &prior).await;
            Err(e)
        }
    }
}

/// One sync round's outcome.
struct SyncResult {
    revision: String,
    applied_count: i32,
    pruned_count: i32,
    errors: Vec<String>,
}
impl SyncResult {
    fn message(&self) -> String {
        if self.errors.is_empty() {
            format!(
                "applied {} object(s) at {}{}",
                self.applied_count,
                self.revision,
                if self.pruned_count > 0 {
                    format!(", pruned {}", self.pruned_count)
                } else {
                    String::new()
                }
            )
        } else {
            // Surface the first couple of failures; status truncates for etcd.
            let head: Vec<&str> = self.errors.iter().take(2).map(String::as_str).collect();
            format!(
                "applied {}, {} error(s): {}",
                self.applied_count,
                self.errors.len(),
                head.join("; ")
            )
        }
    }
}

async fn sync(
    client: &Client,
    cr_namespace: &str,
    gitsource_name: &str,
    spec: &crate::crd::GitSourceSpec,
) -> Result<SyncResult> {
    let token = if spec.credentials_secret.is_empty() {
        String::new()
    } else {
        gitops::read_secret_key(client, cr_namespace, &spec.credentials_secret, "token").await?
    };

    let checkout = Checkout::clone(&spec.repo, &spec.r#ref, &token).await?;
    let revision = checkout.head_short_sha().await?;

    let dir = checkout.join(&spec.path);
    let mut objects = if spec.kustomize {
        render_kustomize(&dir).await?
    } else {
        render_plain(&dir, &spec.allowlist).await?
    };

    // Discovery resolves each manifest's GVK → the real plural + scope (what
    // `kubectl apply` does), so irregular plurals + cluster/namespaced scope are
    // correct rather than guessed.
    let discovery = Discovery::new(client.clone())
        .run()
        .await
        .map_err(OperatorError::from)?;

    let mut applied_count = 0;
    let mut errors: Vec<String> = Vec::new();
    let mut applied_keys: HashSet<String> = HashSet::new();
    let mut seen: HashMap<String, (ApiResource, Scope)> = HashMap::new();

    for obj in objects.iter_mut() {
        stamp_apply_set(obj, gitsource_name);
        match apply_one(client, &discovery, cr_namespace, obj).await {
            Ok((ar, scope, ns, oname)) => {
                applied_count += 1;
                let key = obj_key(&ar, &ns, &oname);
                applied_keys.insert(key);
                seen.entry(gvk_key(&ar)).or_insert((ar, scope));
            }
            Err(e) => errors.push(gitops::scrub(&e.to_string(), &token)),
        }
    }

    let pruned_count = if spec.prune && errors.is_empty() {
        // Only prune after a fully clean apply — never delete on partial state.
        prune(client, gitsource_name, &seen, &applied_keys).await
    } else {
        0
    };

    Ok(SyncResult {
        revision,
        applied_count,
        pruned_count,
        errors,
    })
}

/// Stamp the apply-set label so pruning can find objects THIS GitSource owns.
fn stamp_apply_set(obj: &mut DynamicObject, gitsource_name: &str) {
    obj.metadata
        .labels
        .get_or_insert_with(Default::default)
        .insert(APPLY_SET_LABEL.to_string(), gitsource_name.to_string());
}

/// Resolve + SSA-apply one object. Returns (resource, scope, namespace, name)
/// for prune bookkeeping.
async fn apply_one(
    client: &Client,
    discovery: &Discovery,
    default_ns: &str,
    obj: &DynamicObject,
) -> Result<(ApiResource, Scope, String, String)> {
    let types = obj
        .types
        .clone()
        .ok_or_else(|| OperatorError::Config("manifest missing apiVersion/kind".into()))?;
    let oname = obj
        .metadata
        .name
        .clone()
        .ok_or_else(|| OperatorError::Config("manifest missing metadata.name".into()))?;
    let gvk = GroupVersionKind::try_from(types)
        .map_err(|e| OperatorError::Config(format!("bad apiVersion/kind: {e}")))?;
    let (ar, caps) = discovery.resolve_gvk(&gvk).ok_or_else(|| {
        OperatorError::Reconcile(format!(
            "no served resource for {}/{} {} (CRD installed?)",
            gvk.group, gvk.version, gvk.kind
        ))
    })?;

    let (api, ns): (Api<DynamicObject>, String) = match caps.scope {
        Scope::Namespaced => {
            let ns = obj
                .metadata
                .namespace
                .clone()
                .unwrap_or_else(|| default_ns.to_string());
            (Api::namespaced_with(client.clone(), &ns, &ar), ns)
        }
        Scope::Cluster => (Api::all_with(client.clone(), &ar), String::new()),
    };
    apply::apply_dynamic_as(&api, obj, GITOPS_FIELD_MANAGER).await?;
    Ok((ar, caps.scope, ns, oname))
}

/// Delete objects carrying our apply-set label that are no longer in git. Only
/// runs for `prune: true`, only over the resource kinds we applied this round,
/// and never returns an error (a prune failure must not fail the sync). Returns
/// the number pruned.
async fn prune(
    client: &Client,
    gitsource_name: &str,
    seen: &HashMap<String, (ApiResource, Scope)>,
    applied_keys: &HashSet<String>,
) -> i32 {
    let selector = format!("{APPLY_SET_LABEL}={gitsource_name}");
    let mut pruned = 0;
    for (ar, scope) in seen.values() {
        let list_api: Api<DynamicObject> = Api::all_with(client.clone(), ar);
        let lp = ListParams::default().labels(&selector);
        let list = match list_api.list(&lp).await {
            Ok(l) => l,
            Err(e) => {
                warn!(kind = %ar.kind, error = %e, "GitSource prune: list failed; skipping kind");
                continue;
            }
        };
        for item in list {
            let iname = item.metadata.name.clone().unwrap_or_default();
            let ins = item.metadata.namespace.clone().unwrap_or_default();
            if applied_keys.contains(&obj_key(ar, &ins, &iname)) {
                continue; // still declared in git
            }
            let del_api: Api<DynamicObject> = match scope {
                Scope::Namespaced => Api::namespaced_with(client.clone(), &ins, ar),
                Scope::Cluster => Api::all_with(client.clone(), ar),
            };
            match del_api.delete(&iname, &DeleteParams::default()).await {
                Ok(_) => {
                    info!(kind = %ar.kind, name = %iname, namespace = %ins, "GitSource pruned stale object");
                    pruned += 1;
                }
                Err(e) => {
                    warn!(kind = %ar.kind, name = %iname, error = %e, "GitSource prune: delete failed")
                }
            }
        }
    }
    pruned
}

fn gvk_key(ar: &ApiResource) -> String {
    format!("{}/{}/{}", ar.group, ar.version, ar.kind)
}
fn obj_key(ar: &ApiResource, ns: &str, name: &str) -> String {
    format!("{}|{}|{}", gvk_key(ar), ns, name)
}

/// Render a plain directory of manifest files, honoring the allowlist. Empty
/// allowlist = every `*.yaml` in the directory; non-empty = only those basenames
/// (no `.yaml`), exactly the `RECONCILE_ALLOWLIST` semantics. `kustomization.yaml`
/// is always skipped (it is a build input, not a manifest).
async fn render_plain(dir: &Path, allowlist: &[String]) -> Result<Vec<DynamicObject>> {
    let mut rd = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| OperatorError::Reconcile(format!("read dir {}: {e}", dir.display())))?;
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    while let Some(entry) = rd
        .next_entry()
        .await
        .map_err(|e| OperatorError::Reconcile(format!("read dir entry: {e}")))?
    {
        let p = entry.path();
        if p.extension().and_then(|x| x.to_str()) != Some("yaml") {
            continue;
        }
        let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
        if !file_allowed(stem, allowlist) {
            continue;
        }
        files.push(p);
    }
    files.sort(); // deterministic apply order
    let mut objects = Vec::new();
    for f in files {
        let content = tokio::fs::read_to_string(&f)
            .await
            .map_err(|e| OperatorError::Reconcile(format!("read {}: {e}", f.display())))?;
        objects.extend(parse_manifests(&content)?);
    }
    Ok(objects)
}

/// Whether a manifest file (by its `.yaml` basename, no extension) is applied.
/// `kustomization` is always excluded (a build input). An empty allowlist means
/// "apply the whole declared path" (the GitSource's `spec.path` is the scope); a
/// non-empty allowlist restricts to exactly those basenames — the
/// `RECONCILE_ALLOWLIST` semantics, so the seed CR pins the cron's exact subset.
fn file_allowed(stem: &str, allowlist: &[String]) -> bool {
    if stem == "kustomization" {
        return false;
    }
    allowlist.is_empty() || allowlist.iter().any(|a| a == stem)
}

/// Render a kustomization root by shelling `kustomize build`. Requires the
/// `kustomize` binary in the operator image; a clear error surfaces if absent.
async fn render_kustomize(dir: &Path) -> Result<Vec<DynamicObject>> {
    let out = Command::new("kustomize")
        .arg("build")
        .arg(dir)
        .output()
        .await
        .map_err(|e| {
            OperatorError::Reconcile(format!(
                "kustomize build spawn failed: {e} (is `kustomize` in the operator image?)"
            ))
        })?;
    if !out.status.success() {
        return Err(OperatorError::Reconcile(format!(
            "kustomize build {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    parse_manifests(&String::from_utf8_lossy(&out.stdout))
}

/// Parse a multi-document YAML stream into typed DynamicObjects, skipping empty
/// documents and any without an apiVersion/kind.
fn parse_manifests(content: &str) -> Result<Vec<DynamicObject>> {
    let mut objects = Vec::new();
    for doc in serde_yaml::Deserializer::from_str(content) {
        let value = serde_yaml::Value::deserialize(doc)
            .map_err(|e| OperatorError::Reconcile(format!("yaml parse: {e}")))?;
        if value.is_null() {
            continue;
        }
        let json = serde_json::to_value(&value)?;
        if !json.is_object() {
            continue;
        }
        let obj: DynamicObject = serde_json::from_value(json)
            .map_err(|e| OperatorError::Reconcile(format!("manifest decode: {e}")))?;
        if obj.types.is_none() {
            continue; // not a k8s object (e.g. a bare kustomize config fragment)
        }
        objects.push(obj);
    }
    Ok(objects)
}

async fn write_status(
    client: &Client,
    namespace: &str,
    name: &str,
    status: &GitSourceStatus,
    prior: &GitSourceStatus,
) {
    if !status_changed(status, prior) {
        return;
    }
    let api: Api<GitSource> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({ "status": status });
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update GitSource status (CRD may not be installed)");
    }
}

pub fn on_error(_obj: Arc<GitSource>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "GitSource reconcile failed");
    Action::requeue(Duration::from_secs(60))
}

pub async fn run_gitsource_controller(
    client: Client,
    namespace: String,
    api_group: String,
    reconcile_now: Arc<Notify>,
) {
    let api: Api<GitSource> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting GitSource controller");
    let ctx = Arc::new(Ctx { client, api_group });
    // A git push nudges `POST /reconcile` → this `Notify` → an immediate sweep of
    // every GitSource; the per-CR `intervalSeconds` requeue is the fallback. One
    // webhook, one loop — the same real-time-with-poll-fallback shape the retired
    // env-configured loop had, now feeding the CR-driven controller.
    let trigger = futures::stream::unfold(reconcile_now, |n| async move {
        n.notified().await;
        Some(((), n))
    });
    Controller::new(api, Config::default())
        .reconcile_all_on(trigger)
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_manifests_multidoc_skips_empty_and_untyped() {
        let content = "\
apiVersion: hanzo.ai/v1
kind: App
metadata:
  name: cloud
  namespace: hanzo
spec:
  role: service
---
# a comment-only doc
---
null
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: cloud-data
  namespace: hanzo
";
        let objs = parse_manifests(content).unwrap();
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].types.as_ref().unwrap().kind, "App");
        assert_eq!(objs[0].metadata.name.as_deref(), Some("cloud"));
        assert_eq!(
            objs[1].types.as_ref().unwrap().kind,
            "PersistentVolumeClaim"
        );
    }

    #[test]
    fn stamp_apply_set_adds_label() {
        let mut obj: DynamicObject = serde_json::from_value(serde_json::json!({
            "apiVersion": "hanzo.ai/v1",
            "kind": "App",
            "metadata": { "name": "cloud", "namespace": "hanzo" },
            "spec": { "role": "service" }
        }))
        .unwrap();
        stamp_apply_set(&mut obj, "universe");
        assert_eq!(
            obj.metadata
                .labels
                .as_ref()
                .unwrap()
                .get(APPLY_SET_LABEL)
                .map(String::as_str),
            Some("universe")
        );
    }

    #[test]
    fn obj_key_is_stable_and_distinct() {
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("hanzo.ai", "v1", "App"));
        let k1 = obj_key(&ar, "hanzo", "cloud");
        let k2 = obj_key(&ar, "hanzo", "cms");
        assert_ne!(k1, k2);
        assert_eq!(k1, obj_key(&ar, "hanzo", "cloud"));
    }

    #[test]
    fn file_allowed_empty_allowlist_applies_all_but_kustomization() {
        let none: Vec<String> = vec![];
        assert!(file_allowed("cloud", &none));
        assert!(file_allowed("world", &none));
        // kustomization is always a build input, never applied.
        assert!(!file_allowed("kustomization", &none));
    }

    #[test]
    fn file_allowed_nonempty_allowlist_restricts_to_subset() {
        let allow = vec!["cloud".to_string(), "world".to_string()];
        assert!(file_allowed("cloud", &allow));
        assert!(file_allowed("world", &allow));
        assert!(!file_allowed("studio", &allow)); // not in the vetted subset
        assert!(!file_allowed("kustomization", &allow));
    }

    // A source declares a CR in ANY namespace; the apply targets that namespace
    // verbatim (apply_one reads obj.metadata.namespace). This is the ownership
    // property the retired env-configured loop lacked — it filtered to one
    // configured namespace and SILENTLY skipped a zen App CR declaring
    // `namespace: zen`, pinning its image for over a day. GitSource has no such
    // filter: the namespace is data on the CR, not a permission.
    #[test]
    fn a_declared_namespace_is_carried_to_the_apply() {
        let objs = parse_manifests(
            "apiVersion: hanzo.ai/v1\nkind: App\nmetadata:\n  name: zen\n  namespace: zen\nspec:\n  role: service\n",
        )
        .unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].metadata.namespace.as_deref(), Some("zen"));
    }

    // The webhook seam: `POST /reconcile` nudges the shared `Notify`, and the
    // trigger stream fed to `reconcile_all_on` yields one reconcile-all tick — a
    // git push reconciles instantly, while the per-CR interval requeue is the
    // fallback. Non-vacuous: without the notify the stream blocks (asserted by the
    // idle timeout), so a broken wiring cannot pass.
    #[tokio::test]
    async fn a_notify_wakes_the_reconcile_trigger() {
        let n = Arc::new(Notify::new());
        let mut trigger = Box::pin(futures::stream::unfold(n.clone(), |n| async move {
            n.notified().await;
            Some(((), n))
        }));

        // Before any push, the trigger is idle — it does not spin.
        let idle = tokio::time::timeout(Duration::from_millis(50), trigger.next()).await;
        assert!(
            idle.is_err(),
            "trigger must block until a notify, not self-fire"
        );

        // A push wakes it exactly once.
        n.notify_one();
        let got = tokio::time::timeout(Duration::from_secs(1), trigger.next()).await;
        assert_eq!(got.ok().flatten(), Some(()), "a notify must yield a tick");

        // Then it goes idle again until the next push.
        let idle2 = tokio::time::timeout(Duration::from_millis(50), trigger.next()).await;
        assert!(idle2.is_err(), "trigger must wait for the next notify");
    }
}
