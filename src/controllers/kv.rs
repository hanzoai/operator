//! KV reconciler — newtype facade over the shared datastore machinery. Valkey
//! workloads (hanzoai/kv) declared as a `KV` CR materialize with
//! `Engine::Valkey` pinned by the Kind: a `KV` CR cannot become a PostgreSQL or
//! S3 datastore because the engine is the Kind, not a field.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info};

use crate::core::{OperatorError, Result};
use crate::crd::{Engine, KV};

use super::{datastore, owner_ref_for};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<KV>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("KV has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "KV");
    let ds_spec = cr.spec.0.clone();
    datastore::reconcile_datastore_inner_pub(
        &ctx.client,
        &name,
        &namespace,
        &ds_spec,
        Engine::Valkey,
        owner,
    )
    .await?;
    // Report Ready on the facade CR just like the canonical Datastore does;
    // without this the `KV` CR carries an empty Ready condition when healthy.
    datastore::write_status::<KV>(
        &ctx.client,
        &name,
        &namespace,
        &ds_spec,
        Engine::Valkey,
        cr.metadata.generation.unwrap_or(0),
    )
    .await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

pub fn on_error(_obj: Arc<KV>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "KV reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_kv_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<KV> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting KV controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use crate::controllers::datastore::connection_string_for;
    use crate::crd::{DBSpec, Engine, KVSpec, StorageSpec};

    // A `KV` CR is pinned to Valkey by its Kind — the reconcile always drives
    // `Engine::Valkey`, so the workload speaks the Redis DSN on 6379.
    #[test]
    fn kv_facade_is_valkey() {
        let facade = KVSpec(DBSpec {
            storage: StorageSpec {
                size: "1Gi".to_string(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            connection_string_for(&facade.0, Engine::Valkey, "cache", "hanzo"),
            "redis://cache.hanzo.svc:6379"
        );
    }
}
