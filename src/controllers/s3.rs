//! S3 reconciler — newtype facade over the shared datastore machinery. MinIO
//! workloads (hanzoai/s3) declared as an `S3` CR materialize with
//! `Engine::Minio` pinned by the Kind: an `S3` CR cannot become a different
//! datastore engine because the engine is the Kind, not a field.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info};

use crate::core::{OperatorError, Result};
use crate::crd::{Engine, S3};

use super::{datastore, owner_ref_for};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<S3>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("S3 has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "S3");
    let ds_spec = cr.spec.0.clone();
    datastore::reconcile_datastore_inner_pub(
        &ctx.client,
        &name,
        &namespace,
        &ds_spec,
        Engine::Minio,
        owner,
    )
    .await?;
    // Report Ready on the facade CR just like the canonical Datastore does
    // (the newtype facade previously never wrote status).
    datastore::write_status::<S3>(
        &ctx.client,
        &name,
        &namespace,
        &ds_spec,
        Engine::Minio,
        cr.metadata.generation.unwrap_or(0),
    )
    .await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

pub fn on_error(_obj: Arc<S3>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "S3 reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_s3_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<S3> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting S3 controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use crate::controllers::datastore::connection_string_for;
    use crate::crd::{DBSpec, Engine, S3Spec, StorageSpec};

    // An `S3` CR is pinned to MinIO by its Kind — the reconcile always drives
    // `Engine::Minio`, so the workload speaks the HTTP DSN on 9000.
    #[test]
    fn s3_facade_is_minio() {
        let facade = S3Spec(DBSpec {
            storage: StorageSpec {
                size: "100Gi".to_string(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            connection_string_for(&facade.0, Engine::Minio, "blobs", "hanzo"),
            "http://blobs.hanzo.svc:9000"
        );
    }
}
