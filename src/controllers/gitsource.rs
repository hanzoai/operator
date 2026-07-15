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
//! v0.6.22 lands the CRD + controller registration and validates the spec
//! (observe + requeue). The clone/render/apply body reuses crate::apply and is
//! filled in next — the contract (this CRD) is what everything else hangs off,
//! so it ships first and compiles standalone.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info, warn};

use crate::core::{OperatorError, Result};
use crate::crd::GitSource;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<GitSource>, _ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("GitSource has no namespace".into()))?;
    let spec = &cr.spec;

    if spec.repo.is_empty() {
        warn!(name = %name, namespace = %namespace, "GitSource has empty repo — nothing to sync");
        return Ok(Action::requeue(Duration::from_secs(120)));
    }

    info!(
        name = %name,
        namespace = %namespace,
        repo = %spec.repo,
        git_ref = %spec.r#ref,
        path = %spec.path,
        prune = spec.prune,
        allowlisted = spec.allowlist.len(),
        "GitSource observed (clone/apply body pending; contract shipped v0.6.22)"
    );

    // Requeue at the CR's own cadence so the sync interval is data, not code.
    Ok(Action::requeue(Duration::from_secs(spec.interval_seconds.max(30))))
}

pub fn on_error(_obj: Arc<GitSource>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "GitSource reconcile failed");
    Action::requeue(Duration::from_secs(60))
}

pub async fn run_gitsource_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<GitSource> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting GitSource controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}
