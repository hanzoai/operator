//! DocDB reconciler — facade over the shared datastore machinery. DocDB
//! (FerretDB + PostgreSQL providing the MongoDB wire protocol) workloads
//! declared as a `DocDB` CR materialize with `Engine::Docdb` pinned by the
//! Kind. DocDB composes SQL (`DocDBSpec(SQLSpec(DBSpec))`) because FerretDB runs
//! on Postgres; the controller unwraps to the shared `DBSpec` and emits the
//! single `hanzoai/docdb` StatefulSet (FerretDB + its embedded Postgres).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info};

use crate::core::{OperatorError, Result};
use crate::crd::{DocDB, Engine};

use super::{datastore, owner_ref_for};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub async fn reconcile(cr: Arc<DocDB>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("DocDB has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "DocDB");
    // DocDBSpec(SQLSpec(DBSpec)) — unwrap both newtype layers to the shared spec.
    let ds_spec = cr.spec.0 .0.clone();
    datastore::reconcile_datastore_inner_pub(
        &ctx.client,
        &name,
        &namespace,
        &ds_spec,
        Engine::Docdb,
        owner,
    )
    .await?;
    // Report Ready on the facade CR just like the canonical Datastore does
    // (the newtype facade previously never wrote status).
    datastore::write_status::<DocDB>(
        &ctx.client,
        &name,
        &namespace,
        &ds_spec,
        Engine::Docdb,
        cr.metadata.generation.unwrap_or(0),
    )
    .await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

pub fn on_error(_obj: Arc<DocDB>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "DocDB reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_docdb_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<DocDB> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting DocDB controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|_| async {})
        .await;
}

#[cfg(test)]
mod tests {
    use crate::controllers::datastore::connection_string_for;
    use crate::crd::{DBSpec, DocDBSpec, Engine, SQLSpec, StorageSpec};

    // A `DocDB` CR composes SQL (`DocDBSpec(SQLSpec(DBSpec))`) and is pinned to
    // FerretDB by its Kind — the reconcile unwraps both layers and drives
    // `Engine::Docdb`, so the workload speaks the MongoDB wire DSN on 27017.
    #[test]
    fn docdb_facade_composes_sql_and_is_ferretdb() {
        let facade = DocDBSpec(SQLSpec(DBSpec {
            storage: StorageSpec {
                size: "5Gi".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }));
        // Unwrap the two newtype layers exactly as the reconcile does.
        let inner = &facade.0 .0;
        assert_eq!(
            connection_string_for(inner, Engine::Docdb, "docs", "hanzo"),
            "mongodb://docs.hanzo.svc:27017"
        );
    }
}
