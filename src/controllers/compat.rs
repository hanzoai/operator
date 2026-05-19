//! Compat controllers — the new unbranded Kinds (SQL/KV/DocDB/KMS/IAM/LLM/
//! S3/Indexer/Explorer) are thin facades over Datastore/Service. Each is a
//! single newtype wrapping the appropriate inner spec.
//!
//! Chain/Subnet/Validator are NoOp-for-now stubs — they delegate to the
//! Network controller which already builds the heavy artifacts.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, ResourceExt};
use tracing::{error, info, warn};

use crate::core::{OperatorError, Result};
use crate::crd::{Chain, DocDB, Explorer, Indexer, Subnet, Validator, IAM, KMS, KV, LLM, S3, SQL};

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

// ---- SQL / KV / DocDB / S3 (Datastore facades) ----

macro_rules! datastore_facade {
    ($name:ident, $cr:ty, $kind:literal) => {
        pub async fn $name(cr: Arc<$cr>, ctx: Arc<Ctx>) -> Result<Action> {
            let name = cr.name_any();
            let namespace = cr
                .namespace()
                .ok_or_else(|| OperatorError::Config(format!("{} has no namespace", $kind)))?;
            let api_version = format!("{}/v1", ctx.api_group);
            let owner = super::owner_ref_for(cr.as_ref(), &api_version, $kind);
            crate::controllers::datastore_inner_for_compat(
                &ctx.client,
                &name,
                &namespace,
                &cr.spec.0,
                owner,
            )
            .await?;
            Ok(Action::requeue(Duration::from_secs(60)))
        }
    };
}

datastore_facade!(reconcile_sql, SQL, "SQL");
datastore_facade!(reconcile_kv, KV, "KV");
datastore_facade!(reconcile_docdb, DocDB, "DocDB");
datastore_facade!(reconcile_s3, S3, "S3");

// ---- IAM / KMS / LLM / Indexer / Explorer (Service facades) ----

macro_rules! service_facade {
    ($name:ident, $cr:ty, $kind:literal) => {
        pub async fn $name(cr: Arc<$cr>, ctx: Arc<Ctx>) -> Result<Action> {
            let name = cr.name_any();
            let namespace = cr
                .namespace()
                .ok_or_else(|| OperatorError::Config(format!("{} has no namespace", $kind)))?;
            let api_version = format!("{}/v1", ctx.api_group);
            let owner = super::owner_ref_for(cr.as_ref(), &api_version, $kind);
            crate::controllers::service_inner_for_compat(
                &ctx.client,
                &name,
                &namespace,
                &cr.spec.0,
                owner,
            )
            .await?;
            Ok(Action::requeue(Duration::from_secs(60)))
        }
    };
}

service_facade!(reconcile_iam, IAM, "IAM");
service_facade!(reconcile_kms, KMS, "KMS");
service_facade!(reconcile_llm, LLM, "LLM");
service_facade!(reconcile_indexer, Indexer, "Indexer");
service_facade!(reconcile_explorer, Explorer, "Explorer");

// ---- Chain / Subnet / Validator (NoOp stubs — Network builds the artifacts) ----

pub async fn reconcile_chain(cr: Arc<Chain>, _ctx: Arc<Ctx>) -> Result<Action> {
    info!(name = %cr.name_any(), "Chain reconcile (NoOp — handled by parent Network)");
    Ok(Action::requeue(Duration::from_secs(300)))
}

pub async fn reconcile_subnet(cr: Arc<Subnet>, _ctx: Arc<Ctx>) -> Result<Action> {
    info!(name = %cr.name_any(), "Subnet reconcile (NoOp — handled by parent Network)");
    Ok(Action::requeue(Duration::from_secs(300)))
}

pub async fn reconcile_validator(cr: Arc<Validator>, _ctx: Arc<Ctx>) -> Result<Action> {
    info!(name = %cr.name_any(), "Validator reconcile (NoOp — handled by parent Network)");
    Ok(Action::requeue(Duration::from_secs(300)))
}

// ---- Error handlers + run functions (one per Kind) ----

macro_rules! ctl_runner {
    ($run_fn:ident, $reconcile:path, $cr:ty, $kind:literal) => {
        pub fn $run_fn(client: Client, namespace: String, api_group: String) -> impl std::future::Future<Output = ()> {
            async move {
                let api: Api<$cr> = if namespace.is_empty() {
                    Api::all(client.clone())
                } else {
                    Api::namespaced(client.clone(), &namespace)
                };
                info!(group = %api_group, "Starting {} controller", $kind);
                let ctx = Arc::new(Ctx {
                    client,
                    api_group,
                });
                Controller::new(api, Config::default())
                    .run($reconcile, |_obj, err, _ctx| {
                        error!(error = %err, kind = $kind, "reconcile failed");
                        Action::requeue(Duration::from_secs(30))
                    }, ctx)
                    .for_each(|res| async move {
                        if let Err(e) = res {
                            warn!(error = %e, kind = $kind, "reconcile error");
                        }
                    })
                    .await;
            }
        }
    };
}

ctl_runner!(run_sql_controller, reconcile_sql, SQL, "SQL");
ctl_runner!(run_kv_controller, reconcile_kv, KV, "KV");
ctl_runner!(run_docdb_controller, reconcile_docdb, DocDB, "DocDB");
ctl_runner!(run_s3_controller, reconcile_s3, S3, "S3");
ctl_runner!(run_iam_controller, reconcile_iam, IAM, "IAM");
ctl_runner!(run_kms_controller, reconcile_kms, KMS, "KMS");
ctl_runner!(run_llm_controller, reconcile_llm, LLM, "LLM");
ctl_runner!(
    run_indexer_controller,
    reconcile_indexer,
    Indexer,
    "Indexer"
);
ctl_runner!(
    run_explorer_controller,
    reconcile_explorer,
    Explorer,
    "Explorer"
);
ctl_runner!(run_chain_controller, reconcile_chain, Chain, "Chain");
ctl_runner!(run_subnet_controller, reconcile_subnet, Subnet, "Subnet");
ctl_runner!(
    run_validator_controller,
    reconcile_validator,
    Validator,
    "Validator"
);
