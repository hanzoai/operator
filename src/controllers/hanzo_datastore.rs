//! HanzoDatastore reconciler — backcompat alias for the canonical Datastore
//! Kind. Newtype facade over DatastoreSpec; delegates to the SAME inner
//! handler as Datastore (`datastore::reconcile_datastore_inner_pub`). No
//! duplicated reconcile.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info};

use crate::core::{OperatorError, Result};
use crate::crd::HanzoDatastore;

use super::{datastore, owner_ref_for};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<HanzoDatastore>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("HanzoDatastore has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "HanzoDatastore");
    datastore::reconcile_datastore_inner_pub(&ctx.client, &name, &namespace, &cr.spec.0, owner)
        .await?;
    Ok(Action::requeue(Duration::from_secs(60)))
}

pub fn on_error(_obj: Arc<HanzoDatastore>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "HanzoDatastore reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_hanzo_datastore_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<HanzoDatastore> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting HanzoDatastore controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use crate::crd::{DatastoreSpec, HanzoDatastoreSpec, StorageSpec};

    #[test]
    fn hanzo_datastore_spec_unwraps_to_datastore_spec() {
        let inner = DatastoreSpec {
            type_: "postgresql".to_string(),
            storage: StorageSpec {
                size: "10Gi".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };
        let facade = HanzoDatastoreSpec(inner.clone());
        assert_eq!(facade.0.type_, "postgresql");
        assert_eq!(facade.0.storage.size, "10Gi");
    }
}
