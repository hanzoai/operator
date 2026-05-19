//! Datastore reconciler — dispatches by `spec.type` to PostgreSQL, Valkey,
//! DocDB (FerretDB), MinIO, NATS, or generic datastore engines.
//!
//! Each type runs as a StatefulSet with a headless Service for pod DNS plus
//! a ClusterIP Service for client connections.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::Service as CoreService;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::Api;
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{Datastore as DatastoreCR, DatastoreSpec, DatastoreStatus, ImageSpec, Phase};
use crate::crd_types;
use crate::manifests;

use super::owner_ref_for;
use crate::crd_types::{build_condition, Condition};

fn upsert_condition(conditions: &mut Vec<Condition>, new_cond: Condition) {
    if let Some(slot) = conditions.iter_mut().find(|c| c.type_ == new_cond.type_) {
        *slot = new_cond;
    } else {
        conditions.push(new_cond);
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

/// Resolve the default image for a given datastore type.
fn default_image_for(type_: &str) -> ImageSpec {
    match type_ {
        "postgresql" => ImageSpec {
            repository: "ghcr.io/hanzoai/sql".to_string(),
            tag: "16".to_string(),
            pull_policy: "IfNotPresent".to_string(),
        },
        "valkey" => ImageSpec {
            repository: "ghcr.io/hanzoai/kv".to_string(),
            tag: "8".to_string(),
            pull_policy: "IfNotPresent".to_string(),
        },
        "docdb" => ImageSpec {
            repository: "ghcr.io/hanzoai/docdb".to_string(),
            tag: "latest".to_string(),
            pull_policy: "IfNotPresent".to_string(),
        },
        "minio" => ImageSpec {
            repository: "ghcr.io/hanzoai/s3".to_string(),
            tag: "latest".to_string(),
            pull_policy: "IfNotPresent".to_string(),
        },
        "nats" => ImageSpec {
            repository: "nats".to_string(),
            tag: "2.10".to_string(),
            pull_policy: "IfNotPresent".to_string(),
        },
        _ => ImageSpec {
            repository: "ghcr.io/hanzoai/datastore".to_string(),
            tag: "latest".to_string(),
            pull_policy: "IfNotPresent".to_string(),
        },
    }
}

/// Reconcile a canonical `Datastore` CR.
pub async fn reconcile_datastore(cr: Arc<DatastoreCR>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("Datastore has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "Datastore");
    reconcile_datastore_inner(&ctx.client, &name, &namespace, &cr.spec, owner).await?;
    write_datastore_status(&ctx.client, &name, &namespace, &cr).await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Public alias for use by compat facades.
pub async fn reconcile_datastore_inner_pub(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &DatastoreSpec,
    owner: OwnerReference,
) -> Result<()> {
    reconcile_datastore_inner(client, name, namespace, spec, owner).await
}

async fn reconcile_datastore_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &DatastoreSpec,
    owner: OwnerReference,
) -> Result<()> {
    let image = spec
        .image
        .clone()
        .unwrap_or_else(|| default_image_for(&spec.type_));

    let std_labels = manifests::standard_labels(name, &spec.type_, &spec.part_of, &image.tag);
    let sel_labels = manifests::selector_labels(name);

    let ports = if spec.ports.is_empty() {
        vec![crate::crd::ServicePort {
            name: spec.type_.clone(),
            container_port: default_port_for(&spec.type_),
            service_port: None,
            protocol: "TCP".to_string(),
        }]
    } else {
        spec.ports.clone()
    };

    let env_k8s: Vec<_> = spec.env.iter().map(crd_types::EnvVar::to_k8s).collect();
    let env_from_k8s: Vec<_> = spec
        .env_from
        .iter()
        .map(crd_types::EnvFromSource::to_k8s)
        .collect();
    let vm_k8s: Vec<_> = spec
        .volume_mounts
        .iter()
        .map(crd_types::VolumeMount::to_k8s)
        .collect();
    let main = manifests::build_container(
        name,
        &manifests::image_ref(&image.repository, &image.tag),
        &image.pull_policy,
        spec.command.clone(),
        spec.args.clone(),
        env_k8s,
        env_from_k8s,
        vm_k8s,
        manifests::container_ports(&ports),
        spec.resources.as_ref().map(manifests::to_k8s_resources),
        None,
        None,
    );
    let mut containers = vec![main];
    containers.extend(spec.sidecars.iter().map(crd_types::Container::to_k8s));

    let pvc_template = manifests::build_pvc_template(
        "data",
        &spec.storage.storage_class_name,
        spec.storage.size.as_str(),
    );

    let volumes_k8s: Vec<_> = spec.volumes.iter().map(crd_types::Volume::to_k8s).collect();
    let ips_k8s: Vec<_> = spec
        .image_pull_secrets
        .iter()
        .map(crd_types::LocalObjectReference::to_k8s)
        .collect();

    let mut sts = manifests::build_statefulset(
        name,
        namespace,
        std_labels.clone(),
        sel_labels.clone(),
        Some(spec.replicas.unwrap_or(1)),
        containers,
        volumes_k8s,
        vec![pvc_template],
        ips_k8s,
        &format!("{}-hs", name),
    );
    set_owner(&mut sts.metadata.owner_references, &owner);
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    apply::apply(&stss, &sts).await?;

    // ClusterIP Service for clients.
    let svc_ports = manifests::service_ports(&ports);
    let mut svc = manifests::build_service(
        name,
        namespace,
        std_labels.clone(),
        svc_ports.clone(),
        sel_labels.clone(),
    );
    set_owner(&mut svc.metadata.owner_references, &owner);
    let svcs: Api<CoreService> = Api::namespaced(client.clone(), namespace);
    apply::apply(&svcs, &svc).await?;

    // Headless Service for pod DNS.
    let mut hs = manifests::build_headless_service(
        &format!("{}-hs", name),
        namespace,
        std_labels.clone(),
        svc_ports.clone(),
        sel_labels.clone(),
    );
    set_owner(&mut hs.metadata.owner_references, &owner);
    apply::apply(&svcs, &hs).await?;

    // Service aliases (backward-compatible DNS names).
    for alias in &spec.service_aliases {
        let mut a = manifests::build_service(
            alias,
            namespace,
            std_labels.clone(),
            svc_ports.clone(),
            sel_labels.clone(),
        );
        set_owner(&mut a.metadata.owner_references, &owner);
        apply::apply(&svcs, &a).await?;
    }

    info!(name, namespace, type_ = %spec.type_, "Datastore reconciled");
    Ok(())
}

fn default_port_for(type_: &str) -> i32 {
    match type_ {
        "postgresql" => 5432,
        "valkey" => 6379,
        "docdb" => 27017,
        "minio" => 9000,
        "nats" => 4222,
        _ => 8080,
    }
}

async fn write_datastore_status(client: &Client, name: &str, namespace: &str, cr: &DatastoreCR) {
    use kube::api::{Patch, PatchParams};
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    let sts = match stss.get_opt(name).await {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "failed to fetch StatefulSet for status");
            return;
        }
    };
    let mut status = DatastoreStatus {
        observed_generation: cr.meta().generation.unwrap_or(0),
        ..Default::default()
    };
    if let Some(s) = sts.and_then(|x| x.status) {
        status.ready_replicas = s.ready_replicas.unwrap_or(0);
    }
    let desired = cr.spec.replicas.unwrap_or(1);
    status.phase = Some(if status.ready_replicas >= desired && desired > 0 {
        Phase::Running
    } else if status.ready_replicas > 0 {
        Phase::Degraded
    } else {
        Phase::Creating
    });
    let ready = matches!(status.phase, Some(Phase::Running));
    let cond = build_condition(
        "Ready",
        ready,
        if ready { "Available" } else { "NotReady" },
        &format!("{}/{} replicas ready", status.ready_replicas, desired),
        status.observed_generation,
    );
    upsert_condition(&mut status.conditions, cond);
    let api: Api<DatastoreCR> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({"status": status});
    let pp = PatchParams::apply(apply::FIELD_MANAGER).force();
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update Datastore status");
    }
}

fn set_owner(refs: &mut Option<Vec<OwnerReference>>, owner: &OwnerReference) {
    let v = refs.get_or_insert_with(Vec::new);
    v.retain(|r| r.uid != owner.uid);
    v.push(owner.clone());
}

pub fn on_error(_obj: Arc<DatastoreCR>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "Datastore reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

/// Run the canonical Datastore controller.
pub async fn run_datastore_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<DatastoreCR> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting Datastore controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile_datastore, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "Datastore reconcile error");
            }
        })
        .await;
}
