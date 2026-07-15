//! ImageUpdate reconciler — registry→git image automation (native GitOps).
//!
//! Retires the `notify-universe` GitHub `repository_dispatch` step (in every
//! service's release.yml) plus universe's `image-update.yml`: instead of CI
//! calling back into git to bump a tag, the operator polls `imageRepository`
//! (registry.hanzo.ai — the canonical fleet registry, NOT ghcr) for tags
//! matching `policy` (semver range / `semver:*` / `regex:`), and on a newer one
//! writes the bump into `writebackPath` in git. The GitSource controller then
//! applies it — closing build→push→bump→rollout entirely in-cluster.
//!
//! v0.6.22 lands the CRD + registration and validates the spec (observe +
//! requeue). The registry-list + semver-select + git-commit body is filled in
//! next; shipping the contract first keeps the two GitOps CRDs versioned
//! together.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info, warn};

use crate::core::{OperatorError, Result};
use crate::crd::ImageUpdate;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<ImageUpdate>, _ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("ImageUpdate has no namespace".into()))?;
    let spec = &cr.spec;

    if spec.image_repository.is_empty() || spec.writeback_path.is_empty() {
        warn!(
            name = %name,
            namespace = %namespace,
            "ImageUpdate missing imageRepository or writebackPath — nothing to automate"
        );
        return Ok(Action::requeue(Duration::from_secs(300)));
    }

    info!(
        name = %name,
        namespace = %namespace,
        image = %spec.image_repository,
        policy = %spec.policy,
        writeback = %spec.writeback_path,
        "ImageUpdate observed (poll/select/commit body pending; contract shipped v0.6.22)"
    );

    Ok(Action::requeue(Duration::from_secs(spec.interval_seconds.max(60))))
}

pub fn on_error(_obj: Arc<ImageUpdate>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "ImageUpdate reconcile failed");
    Action::requeue(Duration::from_secs(60))
}

pub async fn run_imageupdate_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<ImageUpdate> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting ImageUpdate controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}
