//! HanzoService reconciler — backcompat alias for the canonical Service Kind.
//! Newtype facade over ServiceSpec; delegates to the SAME inner handler as
//! Service (`service::reconcile_service_inner_pub`). No duplicated reconcile.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info};

use crate::core::{OperatorError, Result};
use crate::crd::HanzoService;

use super::{owner_ref_for, service};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<HanzoService>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("HanzoService has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "HanzoService");
    service::reconcile_service_inner_pub(&ctx.client, &name, &namespace, &cr.spec.0, owner).await?;
    Ok(Action::requeue(Duration::from_secs(60)))
}

pub fn on_error(_obj: Arc<HanzoService>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "HanzoService reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_hanzo_service_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<HanzoService> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting HanzoService controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use crate::crd::{HanzoServiceSpec, ImageSpec, ServiceSpec};

    #[test]
    fn hanzo_service_spec_unwraps_to_service_spec() {
        let inner = ServiceSpec {
            image: ImageSpec {
                repository: "ghcr.io/hanzoai/api".to_string(),
                tag: "v1.2.3".to_string(),
                pull_policy: "IfNotPresent".to_string(),
            },
            ..Default::default()
        };
        let facade = HanzoServiceSpec(inner.clone());
        assert_eq!(facade.0.image.repository, inner.image.repository);
        assert_eq!(facade.0.image.tag, "v1.2.3");
    }
}
