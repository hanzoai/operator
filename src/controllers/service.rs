//! Service reconciler — the load-bearing controller.
//!
//! Watches `Service`. For
//! each CR, materializes: Deployment, Service, Ingress (when enabled), HPA,
//! PDB, NetworkPolicy, and KMSSecret children.
//!
//! ## Critical invariant
//!
//! `spec.env`, `spec.volumes`, and `spec.volumeMounts` MUST be honored
//! on the generated Deployment. The gateway 503 root cause (May 2026) was
//! the legacy Go operator silently dropping these. The Rust port carries
//! tests asserting the round-trip.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::autoscaling::v2::{CrossVersionObjectReference, HorizontalPodAutoscaler};
use k8s_openapi::api::core::v1::{
    ConfigMap, PodSecurityContext, PodTemplateSpec, Service as CoreService,
};
use k8s_openapi::api::networking::v1::{Ingress, NetworkPolicy};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{
    HaSpec, KMSSecretRef, PersistenceSpec, Phase, Service as ServiceCR, ServiceSpec, ServiceStatus,
};
use crate::crd_types;
use crate::manifests;

use super::owner_ref_for;
use crate::crd_types::{build_condition, Condition};

/// Upsert a condition in-place by `type_`.
fn upsert_condition(conditions: &mut Vec<Condition>, new_cond: Condition) {
    if let Some(slot) = conditions.iter_mut().find(|c| c.type_ == new_cond.type_) {
        *slot = new_cond;
    } else {
        conditions.push(new_cond);
    }
}

// ============================================================================
// persistence — durable SeaweedFS-backed SQLite via hanzoai/replicate.
//
// Generalizes the proven console-sqlite wiring (restore initContainer +
// replicate sidecar + replicate-config ConfigMap + app-db PVC) into ONE
// `spec.persistence` field that any Service Kind can set. All builders here
// are pure functions of a resolved `PersistenceSpec` so they are unit-tested
// without a cluster.
// ============================================================================

/// Default `hanzoai/replicate` image. Pinned to the repo's `VERSION` (0.5.13;
/// the `v0.5.13` tag is published to GHCR). Semver-only per house rule —
/// never `:latest`.
const REPLICATE_IMAGE: &str = "ghcr.io/hanzoai/replicate:v0.5.13";
const REPLICATE_CMD: &str = "/usr/local/bin/replicate";
/// Shared volume name for the live DB file (mounted on main + init + sidecar).
const APP_DB_VOLUME: &str = "app-db";
/// Volume name for the mounted `replicate.yml` ConfigMap.
const REPLICATE_CONFIG_VOLUME: &str = "replicate-config";
const REPLICATE_CONFIG_MOUNT: &str = "/etc/replicate";

/// Apply sane defaults to a user-supplied `PersistenceSpec`. The user only
/// has to set `enabled` + `data_dir` (+ `db_path` or `dir_mode`); everything
/// else (endpoint, region, secrets, image) defaults to the in-cluster
/// SeaweedFS convention. `<service-name>` substitutions are resolved here.
fn resolved_persistence(name: &str, p: &PersistenceSpec) -> PersistenceSpec {
    let mut r = p.clone();
    if r.pattern.is_empty() {
        r.pattern = "**/*.db".to_string();
    }
    if r.s3_endpoint.is_empty() {
        r.s3_endpoint = "http://s3.hanzo.svc:9000".to_string();
    }
    if r.s3_region.is_empty() {
        r.s3_region = "us-east-1".to_string();
    }
    if r.credentials_secret.is_empty() {
        r.credentials_secret = "s3-credentials".to_string();
    }
    if r.age_secret.is_empty() {
        r.age_secret = format!("{}-replicate-age", name);
    }
    if r.image.is_empty() {
        r.image = REPLICATE_IMAGE.to_string();
    }
    r
}

/// The ConfigMap name holding `replicate.yml` for this service.
fn replicate_config_name(name: &str) -> String {
    format!("{}-replicate-config", name)
}

/// The PVC name for the shared `app-db` working volume.
fn app_db_pvc_name(name: &str) -> String {
    format!("{}-app-db", name)
}

/// Render `replicate.yml`. Single-DB mode emits a `path:`; `dir_mode` emits
/// `dir:` + `pattern:` + `watch: true` (replicate appends each DB's relative
/// path to the S3 `path` prefix automatically). Creds + age material are
/// injected as `${...}` env so the ConfigMap stays secret-free.
fn render_replicate_yml(p: &PersistenceSpec) -> String {
    let target = if p.dir_mode {
        // The glob MUST be quoted — a bare YAML scalar starting with `*`
        // (e.g. `**/*.db`) is parsed as an alias reference and is invalid.
        format!(
            "    dir: {}\n    pattern: \"{}\"\n    watch: true\n",
            p.data_dir, p.pattern
        )
    } else {
        format!("    path: {}/{}\n", p.data_dir, p.db_path)
    };
    format!(
        "# hanzoai/replicate -- SQLite WAL -> S3 (SeaweedFS), age-encrypted.\n\
         dbs:\n\
         \x20 - \n\
{target}\
         \x20   replicas:\n\
         \x20     - type: s3\n\
         \x20       bucket: {bucket}\n\
         \x20       path: {s3_path}\n\
         \x20       endpoint: {endpoint}\n\
         \x20       region: {region}\n\
         \x20       force-path-style: {fps}\n\
         \x20       access-key-id: ${{S3_ACCESS_KEY_ID}}\n\
         \x20       secret-access-key: ${{S3_SECRET_ACCESS_KEY}}\n\
         \x20       age:\n\
         \x20         identities:\n\
         \x20           - ${{AGE_IDENTITY}}\n\
         \x20         recipients:\n\
         \x20           - ${{AGE_RECIPIENT}}\n",
        target = target,
        bucket = p.bucket,
        s3_path = p.s3_path,
        endpoint = p.s3_endpoint,
        region = p.s3_region,
        fps = p.force_path_style,
    )
}

/// The pod volume for the live DB file: PVC if `storage` is set, else
/// emptyDir. Shared by main + init + sidecar.
fn app_db_volume(name: &str, p: &PersistenceSpec) -> crd_types::Volume {
    let source = if p.storage.is_some() {
        serde_json::json!({
            "persistentVolumeClaim": { "claimName": app_db_pvc_name(name) }
        })
    } else {
        serde_json::json!({ "emptyDir": {} })
    };
    crd_types::Volume {
        name: APP_DB_VOLUME.to_string(),
        source,
    }
}

/// The pod volume that mounts the `replicate.yml` ConfigMap.
fn replicate_config_volume(name: &str) -> crd_types::Volume {
    crd_types::Volume {
        name: REPLICATE_CONFIG_VOLUME.to_string(),
        source: serde_json::json!({ "configMap": { "name": replicate_config_name(name) } }),
    }
}

/// The two volumeMounts every replicate container shares: the live DB dir and
/// the read-only config.
fn replicate_volume_mounts(p: &PersistenceSpec) -> Vec<crd_types::VolumeMount> {
    vec![
        crd_types::VolumeMount {
            name: APP_DB_VOLUME.to_string(),
            mount_path: p.data_dir.clone(),
            sub_path: String::new(),
            read_only: None,
        },
        crd_types::VolumeMount {
            name: REPLICATE_CONFIG_VOLUME.to_string(),
            mount_path: REPLICATE_CONFIG_MOUNT.to_string(),
            sub_path: String::new(),
            read_only: Some(true),
        },
    ]
}

/// S3 creds + age keypair as container env, sourced from the configured
/// Secrets. Shared by the restore init and the replication sidecar.
fn replicate_env(p: &PersistenceSpec) -> Vec<crd_types::EnvVar> {
    let secret_ref = |secret: &str, key: &str| crd_types::EnvVarSource {
        secret_key_ref: Some(crd_types::SecretKeySelector {
            name: secret.to_string(),
            key: key.to_string(),
            optional: None,
        }),
        ..Default::default()
    };
    vec![
        crd_types::EnvVar {
            name: "S3_ACCESS_KEY_ID".to_string(),
            value: None,
            value_from: Some(secret_ref(&p.credentials_secret, "access-key")),
        },
        crd_types::EnvVar {
            name: "S3_SECRET_ACCESS_KEY".to_string(),
            value: None,
            value_from: Some(secret_ref(&p.credentials_secret, "secret-key")),
        },
        crd_types::EnvVar {
            name: "AGE_IDENTITY".to_string(),
            value: None,
            value_from: Some(secret_ref(&p.age_secret, "identity")),
        },
        crd_types::EnvVar {
            name: "AGE_RECIPIENT".to_string(),
            value: None,
            value_from: Some(secret_ref(&p.age_secret, "recipients")),
        },
    ]
}

/// The `replicate-restore` initContainer (single-DB mode only — see
/// `dir_mode` handling at the callsite). No-op (exit 0) when `app.db` already
/// exists on the volume or no snapshot exists yet in the bucket.
fn replicate_restore_init(p: &PersistenceSpec) -> crd_types::Container {
    crd_types::Container {
        name: "replicate-restore".to_string(),
        image: p.image.clone(),
        command: vec![REPLICATE_CMD.to_string()],
        args: vec![
            "restore".to_string(),
            "-config".to_string(),
            format!("{}/replicate.yml", REPLICATE_CONFIG_MOUNT),
            "-if-db-not-exists".to_string(),
            "-if-replica-exists".to_string(),
            format!("{}/{}", p.data_dir, p.db_path),
        ],
        env: replicate_env(p),
        env_from: vec![],
        volume_mounts: replicate_volume_mounts(p),
        image_pull_policy: "IfNotPresent".to_string(),
    }
}

/// The `replicate` sidecar: continuously stream the SQLite WAL to S3,
/// age-encrypting client-side. Shares `app-db` with the main container.
fn replicate_sidecar(p: &PersistenceSpec) -> crd_types::Container {
    crd_types::Container {
        name: "replicate".to_string(),
        image: p.image.clone(),
        command: vec![REPLICATE_CMD.to_string()],
        args: vec![
            "replicate".to_string(),
            "-config".to_string(),
            format!("{}/replicate.yml", REPLICATE_CONFIG_MOUNT),
        ],
        env: replicate_env(p),
        env_from: vec![],
        volume_mounts: replicate_volume_mounts(p),
        image_pull_policy: "IfNotPresent".to_string(),
    }
}

/// The main container's mount of the shared `app-db` volume, so the app
/// reads/writes the same DB file the sidecar streams.
fn main_app_db_mount(p: &PersistenceSpec) -> crd_types::VolumeMount {
    crd_types::VolumeMount {
        name: APP_DB_VOLUME.to_string(),
        mount_path: p.data_dir.clone(),
        sub_path: String::new(),
        read_only: None,
    }
}

// ============================================================================
// HA — zero-downtime SQLite topology (StatefulSet + per-pod PVC + headless +
// primary-only Service). The operator expresses the TOPOLOGY; the app owns the
// replication mechanism (in-process hanzoai/replicate: WAL→S3, s3.Leaser).
// ============================================================================

/// Default per-pod data volume name (`volumeClaimTemplate` + main mount).
const HA_DEFAULT_VOLUME: &str = "data";

/// Resolve an [`HaSpec`] with defaults filled in.
fn resolved_ha(h: &HaSpec) -> HaSpec {
    let mut r = h.clone();
    if r.volume_name.is_empty() {
        r.volume_name = HA_DEFAULT_VOLUME.to_string();
    }
    r
}

/// The main container's mount of the per-pod HA data volume (backed by the
/// StatefulSet `volumeClaimTemplate`), so each pod reads/writes its OWN PVC.
fn ha_data_mount(h: &HaSpec) -> crd_types::VolumeMount {
    crd_types::VolumeMount {
        name: h.volume_name.clone(),
        mount_path: h.data_dir.clone(),
        sub_path: String::new(),
        read_only: None,
    }
}

/// Apply pod-template extras shared by the Deployment and StatefulSet render
/// paths: template annotations, init containers (+ the persistence restore
/// init in single-DB mode), and the opt-in `securityContext.fsGroup`. Kept in
/// one place so both workload kinds stay byte-identical in these fields.
fn apply_pod_template_extras(
    tpl: &mut PodTemplateSpec,
    spec: &ServiceSpec,
    persistence: &Option<PersistenceSpec>,
) {
    if let Some(annotations) = &spec.annotations {
        if let Some(meta) = tpl.metadata.as_mut() {
            meta.annotations = Some(annotations.clone());
        }
    }
    // Spec init containers, plus the auto-injected replicate-restore init.
    // dir_mode omits the restore init — directory restore is best-effort via
    // the sidecar's restore-on-boot (a single file path can't address a
    // fan-out of per-org/user DBs).
    let mut inits: Vec<_> = spec
        .init_containers
        .iter()
        .map(crd_types::Container::to_k8s)
        .collect();
    if let Some(p) = persistence {
        if !p.dir_mode {
            inits.push(replicate_restore_init(p).to_k8s());
        }
    }
    if !inits.is_empty() {
        if let Some(pod) = tpl.spec.as_mut() {
            pod.init_containers = Some(inits);
        }
    }
    // Pod securityContext.fsGroup — opt-in (spec.fsGroup). Lets a non-root
    // image write a persistence PVC (the kubelet chowns the volume to this
    // GID + adds it to every container's supplementary groups).
    if let Some(fsg) = spec.fs_group {
        if let Some(pod) = tpl.spec.as_mut() {
            pod.security_context = Some(PodSecurityContext {
                fs_group: Some(fsg),
                ..Default::default()
            });
        }
    }
}

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

/// Reconcile a canonical `Service` CR.
pub async fn reconcile_service(cr: Arc<ServiceCR>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("Service has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "Service");

    reconcile_service_inner(&ctx.client, &name, &namespace, &cr.spec, owner).await?;

    // Status writeback: poll the backing workload for ready replica count —
    // a StatefulSet in HA mode, else the Deployment.
    let mut status = ServiceStatus {
        observed_generation: cr.meta().generation.unwrap_or(0),
        ..Default::default()
    };
    if cr.spec.ha.as_ref().is_some_and(|h| h.enabled) {
        let sts_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &namespace);
        if let Some(s) = sts_api.get_opt(&name).await?.and_then(|x| x.status) {
            status.ready_replicas = s.ready_replicas.unwrap_or(0);
            status.available_replicas = s.available_replicas.unwrap_or(0);
        }
    } else {
        let dep_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &namespace);
        if let Some(s) = dep_api.get_opt(&name).await?.and_then(|x| x.status) {
            status.ready_replicas = s.ready_replicas.unwrap_or(0);
            status.available_replicas = s.available_replicas.unwrap_or(0);
        }
    }
    let desired_replicas = cr.spec.replicas.unwrap_or(1);

    let phase = if status.ready_replicas >= desired_replicas && desired_replicas > 0 {
        Phase::Running
    } else if status.ready_replicas > 0 {
        Phase::Degraded
    } else {
        Phase::Creating
    };
    status.phase = Some(phase.clone());

    let ready = matches!(phase, Phase::Running);
    let cond = build_condition(
        "Ready",
        ready,
        if ready { "Available" } else { "NotReady" },
        &format!(
            "{}/{} replicas ready",
            status.ready_replicas, desired_replicas
        ),
        status.observed_generation,
    );
    upsert_condition(&mut status.conditions, cond);

    // Compute endpoint URLs from ingress hosts.
    if let Some(ing) = &cr.spec.ingress {
        if ing.enabled {
            let scheme = if ing.tls { "https" } else { "http" };
            status.endpoints = ing
                .hosts
                .iter()
                .map(|h| format!("{}://{}", scheme, h))
                .collect();
        }
    }

    let api: Api<ServiceCR> = Api::namespaced(ctx.client.clone(), &namespace);
    let patch = serde_json::json!({"status": status});
    let pp = PatchParams::apply(apply::FIELD_MANAGER).force();
    if let Err(e) = api.patch_status(&name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update Service status (CRD may not be installed)");
    }

    Ok(Action::requeue(Duration::from_secs(60)))
}

/// Public alias for use by compat facades.
pub async fn reconcile_service_inner_pub(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &ServiceSpec,
    owner: OwnerReference,
) -> Result<()> {
    reconcile_service_inner(client, name, namespace, spec, owner).await
}

/// Shared implementation. Materializes Deployment + Service + Ingress +
/// HPA + PDB + NetworkPolicy + KMSSecret children.
async fn reconcile_service_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &ServiceSpec,
    owner: OwnerReference,
) -> Result<()> {
    let std_labels =
        manifests::standard_labels(name, &spec.component, &spec.part_of, &spec.image.tag);
    let sel_labels = manifests::selector_labels(name);
    let extra_labels = spec.labels.clone().unwrap_or_default();
    let all_labels = manifests::merge_labels(&[&std_labels, &extra_labels]);

    // Resolve HA topology first. HA is orthogonal to (and never combined with)
    // sidecar persistence — both would ship the same WAL. When HA is enabled
    // the app owns replication in-process (hanzoai/replicate), so sidecar
    // persistence is suppressed.
    let ha = spec.ha.as_ref().filter(|h| h.enabled).map(resolved_ha);

    // Resolve persistence (defaults filled in) when enabled AND HA is off.
    // Drives the auto-injected app-db mount, ConfigMap, restore init, sidecar.
    let persistence = if ha.is_some() {
        None
    } else {
        spec.persistence
            .as_ref()
            .filter(|p| p.enabled)
            .map(|p| resolved_persistence(name, p))
    };

    // 1. Build the main container honoring spec.env/volumes/volumeMounts.
    let env_k8s: Vec<_> = spec.env.iter().map(crd_types::EnvVar::to_k8s).collect();
    let env_from_k8s: Vec<_> = spec
        .env_from
        .iter()
        .map(crd_types::EnvFromSource::to_k8s)
        .collect();
    // Honor spec.volume_mounts, then auto-inject the shared data mount on the
    // MAIN container: the per-pod HA PVC (StatefulSet volumeClaimTemplate) in
    // HA mode, else the sidecar-shared app-db volume in persistence mode.
    let mut main_vms: Vec<crd_types::VolumeMount> = spec.volume_mounts.clone();
    if let Some(h) = &ha {
        main_vms.push(ha_data_mount(h));
    } else if let Some(p) = &persistence {
        main_vms.push(main_app_db_mount(p));
    }
    let vm_k8s: Vec<_> = main_vms
        .iter()
        .map(crd_types::VolumeMount::to_k8s)
        .collect();
    let mut main = manifests::build_container(
        name,
        &manifests::image_ref(&spec.image.repository, &spec.image.tag),
        &spec.image.pull_policy,
        spec.command.clone(),
        spec.args.clone(),
        env_k8s,
        env_from_k8s,
        vm_k8s,
        manifests::container_ports(&spec.ports),
        spec.resources.as_ref().map(manifests::to_k8s_resources),
        spec.liveness_probe
            .as_ref()
            .map(manifests::build_http_probe),
        spec.readiness_probe
            .as_ref()
            .map(manifests::build_http_probe),
    );
    // Configurable preStop (HA drain: checkpoint → final Sync → release lease).
    // Pre-set on the MAIN container so the default-injecting builder leaves it
    // untouched; every other container keeps the byte-identical `sleep 5`.
    if !spec.pre_stop.is_empty() {
        main.lifecycle = Some(manifests::pre_stop_lifecycle(&spec.pre_stop));
    }
    let mut containers = vec![main];
    containers.extend(spec.sidecars.iter().map(crd_types::Container::to_k8s));
    // Auto-inject the replicate sidecar (persistence mode only; HA ships
    // in-process, so no sidecar — two shippers would corrupt the WAL stream).
    if let Some(p) = &persistence {
        containers.push(replicate_sidecar(p).to_k8s());
    }

    // 2. Assemble pod volumes + image pull secrets.
    let mut all_volumes: Vec<crd_types::Volume> = spec.volumes.clone();
    if let Some(p) = &persistence {
        // Shared live-DB volume (PVC or emptyDir) + the replicate.yml mount.
        all_volumes.push(app_db_volume(name, p));
        all_volumes.push(replicate_config_volume(name));
    }
    // HA mode adds NO pod volume for data — the volumeClaimTemplate provides a
    // per-pod PVC bound to `ha.volume_name` (no shared-RWO deadlock).
    let volumes_k8s: Vec<_> = all_volumes.iter().map(crd_types::Volume::to_k8s).collect();
    let ips_k8s: Vec<_> = spec
        .image_pull_secrets
        .iter()
        .map(crd_types::LocalObjectReference::to_k8s)
        .collect();

    // 3. Render the workload: a StatefulSet (per-pod PVC → no shared-RWO
    // deadlock, so `replicas: 2` primary+standby is possible) when HA is
    // enabled, else the Deployment path (unchanged / byte-identical).
    if let Some(h) = &ha {
        let pvc = manifests::build_pvc_template(
            &h.volume_name,
            &h.storage.storage_class_name,
            h.storage.size.as_str(),
        );
        let mut sts = manifests::build_statefulset(
            name,
            namespace,
            all_labels.clone(),
            sel_labels.clone(),
            Some(spec.replicas.unwrap_or(1)),
            containers,
            volumes_k8s,
            vec![pvc],
            ips_k8s,
            &format!("{}-hs", name),
        );
        if let Some(s_spec) = sts.spec.as_mut() {
            // serviceAccountName lives on the pod spec (build_statefulset takes
            // no SA arg, unlike build_deployment).
            if !spec.service_account_name.is_empty() {
                if let Some(pod) = s_spec.template.spec.as_mut() {
                    pod.service_account_name = Some(spec.service_account_name.clone());
                }
            }
            apply_pod_template_extras(&mut s_spec.template, spec, &persistence);
        }
        set_owner(&mut sts.metadata.owner_references, &owner);
        let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
        apply::apply(&stss, &sts).await?;

        // Headless Service for stable per-pod DNS (StatefulSet requirement) +
        // the primary-only Service that routes writes to the lease holder.
        if !spec.ports.is_empty() {
            let svc_ports = manifests::service_ports(&spec.ports);
            let svcs: Api<CoreService> = Api::namespaced(client.clone(), namespace);
            let mut hs = manifests::build_headless_service(
                &format!("{}-hs", name),
                namespace,
                all_labels.clone(),
                svc_ports.clone(),
                sel_labels.clone(),
            );
            set_owner(&mut hs.metadata.owner_references, &owner);
            apply::apply(&svcs, &hs).await?;

            if h.primary_service {
                let mut ps = manifests::build_primary_service(
                    &format!("{}-primary", name),
                    namespace,
                    all_labels.clone(),
                    svc_ports,
                    sel_labels.clone(),
                );
                set_owner(&mut ps.metadata.owner_references, &owner);
                apply::apply(&svcs, &ps).await?;
            }
        }
    } else {
        // Deployment path (unchanged).
        //
        // When HPA is enabled, the operator MUST NOT own `spec.replicas` —
        // server-side apply would otherwise fight the HPA every reconcile.
        // Passing `None` removes the field so the HPA is the sole field manager
        // for replicas; the initial scale is `spec.autoscaling.minReplicas`.
        let replicas_for_deployment = if spec.autoscaling.as_ref().is_some_and(|a| a.enabled) {
            None
        } else {
            Some(spec.replicas.unwrap_or(1))
        };
        let mut deploy = manifests::build_deployment(
            name,
            namespace,
            all_labels.clone(),
            sel_labels.clone(),
            replicas_for_deployment,
            containers,
            volumes_k8s,
            &spec.strategy,
            ips_k8s,
            &spec.service_account_name,
        );
        if let Some(d_spec) = deploy.spec.as_mut() {
            apply_pod_template_extras(&mut d_spec.template, spec, &persistence);
        }
        set_owner(&mut deploy.metadata.owner_references, &owner);
        let deps: Api<Deployment> = Api::namespaced(client.clone(), namespace);
        apply::apply(&deps, &deploy).await?;
    }

    // 2b. Persistence ConfigMap (`replicate.yml`). Owned by the Service so it
    // is GC'd with the CR.
    if let Some(p) = &persistence {
        let mut cm_data = std::collections::BTreeMap::new();
        cm_data.insert("replicate.yml".to_string(), render_replicate_yml(p));
        let mut cm = manifests::build_configmap(
            &replicate_config_name(name),
            namespace,
            all_labels.clone(),
            cm_data,
        );
        set_owner(&mut cm.metadata.owner_references, &owner);
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
        apply::apply(&cms, &cm).await?;
    }

    // 3. Service (only if ports are defined).
    if !spec.ports.is_empty() {
        let svc_ports = manifests::service_ports(&spec.ports);
        let mut svc = manifests::build_service(
            name,
            namespace,
            all_labels.clone(),
            svc_ports,
            sel_labels.clone(),
        );
        set_owner(&mut svc.metadata.owner_references, &owner);
        let svcs: Api<CoreService> = Api::namespaced(client.clone(), namespace);
        apply::apply(&svcs, &svc).await?;
    }

    // 4. Ingress.
    if let Some(ing_spec) = &spec.ingress {
        if ing_spec.enabled && !spec.ports.is_empty() {
            let port = manifests::primary_port(&spec.ports);
            let mut ing =
                manifests::build_ingress(name, namespace, ing_spec, name, port, all_labels.clone());
            set_owner(&mut ing.metadata.owner_references, &owner);
            let ings: Api<Ingress> = Api::namespaced(client.clone(), namespace);
            apply::apply(&ings, &ing).await?;
        }
    }

    // 5. HPA. Skipped in HA mode — HPA targets a Deployment, which HA replaces
    // with a StatefulSet (and HA runs a fixed primary+standby set).
    if let Some(as_spec) = &spec.autoscaling {
        if as_spec.enabled && ha.is_none() {
            let target = CrossVersionObjectReference {
                api_version: Some("apps/v1".to_string()),
                kind: "Deployment".to_string(),
                name: name.to_string(),
            };
            let mut hpa =
                manifests::build_hpa(name, namespace, target, as_spec, all_labels.clone());
            set_owner(&mut hpa.metadata.owner_references, &owner);
            let hpas: Api<HorizontalPodAutoscaler> = Api::namespaced(client.clone(), namespace);
            apply::apply(&hpas, &hpa).await?;
        }
    }

    // 6. PDB.
    if let Some(pdb_spec) = &spec.pdb {
        if pdb_spec.enabled {
            let mut pdb = manifests::build_pdb(
                name,
                namespace,
                pdb_spec,
                sel_labels.clone(),
                all_labels.clone(),
            );
            set_owner(&mut pdb.metadata.owner_references, &owner);
            let pdbs: Api<PodDisruptionBudget> = Api::namespaced(client.clone(), namespace);
            apply::apply(&pdbs, &pdb).await?;
        }
    }

    // 7. NetworkPolicy.
    if let Some(np_spec) = &spec.network_policy {
        if np_spec.enabled.unwrap_or(true) {
            let mut np = manifests::build_network_policy(
                name,
                namespace,
                np_spec,
                sel_labels.clone(),
                all_labels.clone(),
            );
            set_owner(&mut np.metadata.owner_references, &owner);
            let nps: Api<NetworkPolicy> = Api::namespaced(client.clone(), namespace);
            apply::apply(&nps, &np).await?;
        }
    }

    // 8. KMSSecret children (dynamic — written via DynamicObject so we
    // don't depend on the KMS CRD types being known to this binary).
    for ref_spec in &spec.kms_secrets {
        if let Err(e) = reconcile_kms_secret(client, namespace, ref_spec, &owner, &all_labels).await
        {
            warn!(name = %ref_spec.managed_secret_name, error = %e, "KMSSecret reconcile failed (CRD may not be installed)");
        }
    }

    info!(name, namespace, "Service reconciled");
    Ok(())
}

/// Write a KMSSecret CR as a DynamicObject. The KMS CRD lives in
/// `kms.hanzo.ai` and is reconciled by the KMS operator — this controller
/// only declares the desired state.
async fn reconcile_kms_secret(
    client: &Client,
    namespace: &str,
    ref_spec: &KMSSecretRef,
    owner: &OwnerReference,
    labels: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    use kube::core::{ApiResource, DynamicObject, GroupVersionKind};

    let gvk = GroupVersionKind::gvk("kms.hanzo.ai", "v1alpha1", "KMSSecret");
    let ar = ApiResource::from_gvk(&gvk);
    let kms_api: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);

    let mut obj = DynamicObject::new(&ref_spec.managed_secret_name, &ar);
    obj.metadata.namespace = Some(namespace.to_string());
    obj.metadata.labels = Some(labels.clone());
    obj.metadata.owner_references = Some(vec![owner.clone()]);

    let spec_json = serde_json::json!({
        "hostAPI": ref_spec.host_api,
        "projectSlug": ref_spec.project_slug,
        "envSlug": ref_spec.env_slug,
        "secretsPath": ref_spec.secrets_path,
        "credentialsRef": {
            "name": ref_spec.credentials_ref.name,
            "namespace": ref_spec.credentials_ref.namespace,
        },
        "resyncInterval": ref_spec.resync_interval,
        "managedSecretName": ref_spec.managed_secret_name,
    });
    obj.data = serde_json::json!({ "spec": spec_json });
    apply::apply_dynamic(&kms_api, &obj).await?;
    Ok(())
}

fn set_owner(refs: &mut Option<Vec<OwnerReference>>, owner: &OwnerReference) {
    let v = refs.get_or_insert_with(Vec::new);
    v.retain(|r| r.uid != owner.uid);
    v.push(owner.clone());
}

pub fn on_error_service(_obj: Arc<ServiceCR>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "Service reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

/// Run the canonical Service controller.
pub async fn run_service_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<ServiceCR> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting Service controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile_service, on_error_service, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "Service reconcile error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{AutoscalingSpec, ImageSpec, ServicePort as CrServicePort};

    fn base_spec() -> ServiceSpec {
        ServiceSpec {
            image: ImageSpec {
                repository: "ghcr.io/hanzoai/test".to_string(),
                tag: "v1.0.0".to_string(),
                pull_policy: "IfNotPresent".to_string(),
            },
            replicas: Some(2),
            ports: vec![CrServicePort {
                name: "http".to_string(),
                container_port: 8080,
                service_port: None,
                protocol: "TCP".to_string(),
            }],
            env: vec![crd_types::EnvVar {
                name: "FOO".to_string(),
                value: Some("bar".to_string()),
                value_from: None,
            }],
            volumes: vec![crd_types::Volume {
                name: "data".to_string(),
                source: serde_json::json!({}),
            }],
            volume_mounts: vec![crd_types::VolumeMount {
                name: "data".to_string(),
                mount_path: "/data".to_string(),
                sub_path: String::new(),
                read_only: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn env_is_carried_to_main_container() {
        // CRITICAL: spec.env MUST appear on the generated Deployment's main
        // container. Gateway 503 root cause was this not happening.
        let spec = base_spec();
        let env_k8s: Vec<_> = spec.env.iter().map(crd_types::EnvVar::to_k8s).collect();
        let vm_k8s: Vec<_> = spec
            .volume_mounts
            .iter()
            .map(crd_types::VolumeMount::to_k8s)
            .collect();
        let main = manifests::build_container(
            "test",
            &manifests::image_ref(&spec.image.repository, &spec.image.tag),
            &spec.image.pull_policy,
            spec.command.clone(),
            spec.args.clone(),
            env_k8s,
            vec![],
            vm_k8s,
            manifests::container_ports(&spec.ports),
            None,
            None,
            None,
        );
        let env = main.env.expect("env must be set");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].name, "FOO");
        assert_eq!(env[0].value.as_deref(), Some("bar"));
    }

    #[test]
    fn volume_mounts_are_carried_to_main_container() {
        let spec = base_spec();
        let vm_k8s: Vec<_> = spec
            .volume_mounts
            .iter()
            .map(crd_types::VolumeMount::to_k8s)
            .collect();
        let main = manifests::build_container(
            "test",
            &manifests::image_ref(&spec.image.repository, &spec.image.tag),
            "",
            vec![],
            vec![],
            vec![],
            vec![],
            vm_k8s,
            vec![],
            None,
            None,
            None,
        );
        let vms = main.volume_mounts.expect("volume_mounts must be set");
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].mount_path, "/data");
    }

    #[test]
    fn deployment_carries_volumes() {
        let spec = base_spec();
        let labels = manifests::standard_labels("test", "", "", "v1.0.0");
        let sel = manifests::selector_labels("test");
        let vols_k8s: Vec<_> = spec.volumes.iter().map(crd_types::Volume::to_k8s).collect();
        let dep = manifests::build_deployment(
            "test",
            "default",
            labels,
            sel,
            Some(2),
            vec![],
            vols_k8s,
            "",
            vec![],
            "",
        );
        let pod_spec = dep.spec.unwrap().template.spec.unwrap();
        let vols = pod_spec.volumes.expect("volumes must be on pod spec");
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].name, "data");
    }

    // ---- Replicas / HPA interaction ----

    /// Helper that mirrors the runtime logic in `reconcile_service_inner` for
    /// deciding what to pass to `build_deployment` as `replicas`. Keep this
    /// function in lockstep with the controller body.
    fn replicas_for_deployment(spec: &ServiceSpec) -> Option<i32> {
        if spec.autoscaling.as_ref().is_some_and(|a| a.enabled) {
            None
        } else {
            Some(spec.replicas.unwrap_or(1))
        }
    }

    #[test]
    fn deployment_omits_replicas_when_autoscaling_enabled() {
        // When HPA is enabled the operator must NOT own spec.replicas.
        // Server-side apply would otherwise fight the HPA every reconcile.
        let mut spec = base_spec();
        spec.replicas = Some(2);
        spec.autoscaling = Some(AutoscalingSpec {
            enabled: true,
            min_replicas: Some(2),
            max_replicas: Some(20),
            target_cpu_utilization: Some(70),
            target_memory_utilization: None,
        });
        assert_eq!(
            replicas_for_deployment(&spec),
            None,
            "with HPA enabled, deployment.replicas must be None so HPA owns the field"
        );
    }

    #[test]
    fn deployment_keeps_replicas_when_autoscaling_disabled() {
        let mut spec = base_spec();
        spec.replicas = Some(3);
        spec.autoscaling = Some(AutoscalingSpec {
            enabled: false,
            min_replicas: None,
            max_replicas: None,
            target_cpu_utilization: None,
            target_memory_utilization: None,
        });
        assert_eq!(replicas_for_deployment(&spec), Some(3));
    }

    #[test]
    fn deployment_keeps_replicas_when_autoscaling_unset() {
        let mut spec = base_spec();
        spec.replicas = Some(4);
        spec.autoscaling = None;
        assert_eq!(replicas_for_deployment(&spec), Some(4));
    }

    #[test]
    fn deployment_defaults_to_one_replica_when_unset_and_no_hpa() {
        let mut spec = base_spec();
        spec.replicas = None;
        spec.autoscaling = None;
        assert_eq!(replicas_for_deployment(&spec), Some(1));
    }

    // ---- persistence (SeaweedFS-backed SQLite via hanzoai/replicate) ----

    use crate::crd::{PersistenceSpec, StorageSpec};

    /// Single-DB persistence spec (the console-sqlite shape): only the fields
    /// a user would set — defaults fill in endpoint/region/secrets/image.
    fn persistence_spec() -> PersistenceSpec {
        PersistenceSpec {
            enabled: true,
            data_dir: "/var/lib/hanzo/console".to_string(),
            db_path: "app.db".to_string(),
            bucket: "console-db".to_string(),
            s3_path: "console/app".to_string(),
            // serde default for the field is `true` (see `default = "default_true"`);
            // set it here so this hand-built spec matches what a CR deserializes to.
            force_path_style: true,
            storage: Some(StorageSpec {
                size: "10Gi".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Assemble the Deployment exactly as `reconcile_service_inner` does for a
    /// Service with persistence enabled — the same resolution + helper calls,
    /// fed into the same `build_deployment` (mirrors `deployment_carries_volumes`).
    fn build_persisted_deployment(
        name: &str,
        spec: &ServiceSpec,
    ) -> k8s_openapi::api::apps::v1::Deployment {
        let p = spec
            .persistence
            .as_ref()
            .filter(|p| p.enabled)
            .map(|p| resolved_persistence(name, p));

        let mut main_vms: Vec<crd_types::VolumeMount> = spec.volume_mounts.clone();
        if let Some(p) = &p {
            main_vms.push(main_app_db_mount(p));
        }
        let vm_k8s: Vec<_> = main_vms
            .iter()
            .map(crd_types::VolumeMount::to_k8s)
            .collect();
        let main = manifests::build_container(
            name,
            &manifests::image_ref(&spec.image.repository, &spec.image.tag),
            &spec.image.pull_policy,
            spec.command.clone(),
            spec.args.clone(),
            spec.env.iter().map(crd_types::EnvVar::to_k8s).collect(),
            vec![],
            vm_k8s,
            manifests::container_ports(&spec.ports),
            None,
            None,
            None,
        );
        let mut containers = vec![main];
        containers.extend(spec.sidecars.iter().map(crd_types::Container::to_k8s));
        if let Some(p) = &p {
            containers.push(replicate_sidecar(p).to_k8s());
        }

        let mut all_volumes: Vec<crd_types::Volume> = spec.volumes.clone();
        if let Some(p) = &p {
            all_volumes.push(app_db_volume(name, p));
            all_volumes.push(replicate_config_volume(name));
        }
        let volumes_k8s: Vec<_> = all_volumes.iter().map(crd_types::Volume::to_k8s).collect();

        let mut deploy = manifests::build_deployment(
            name,
            "hanzo",
            manifests::standard_labels(name, "", "", &spec.image.tag),
            manifests::selector_labels(name),
            Some(1),
            containers,
            volumes_k8s,
            "Recreate",
            vec![],
            "",
        );
        if let Some(d_spec) = deploy.spec.as_mut() {
            let mut inits: Vec<_> = spec
                .init_containers
                .iter()
                .map(crd_types::Container::to_k8s)
                .collect();
            if let Some(p) = &p {
                if !p.dir_mode {
                    inits.push(replicate_restore_init(p).to_k8s());
                }
            }
            if !inits.is_empty() {
                if let Some(pod) = d_spec.template.spec.as_mut() {
                    pod.init_containers = Some(inits);
                }
            }
        }
        deploy
    }

    #[test]
    fn persistence_emits_replicate_restore_init() {
        let mut spec = base_spec();
        spec.persistence = Some(persistence_spec());
        let dep = build_persisted_deployment("console", &spec);
        let inits = dep
            .spec
            .unwrap()
            .template
            .spec
            .unwrap()
            .init_containers
            .expect("init containers must be present");
        assert!(
            inits.iter().any(|c| c.name == "replicate-restore"),
            "single-DB persistence must inject a replicate-restore initContainer"
        );
    }

    #[test]
    fn persistence_emits_replicate_sidecar() {
        let mut spec = base_spec();
        spec.persistence = Some(persistence_spec());
        let dep = build_persisted_deployment("console", &spec);
        let containers = dep.spec.unwrap().template.spec.unwrap().containers;
        assert!(
            containers.iter().any(|c| c.name == "replicate"),
            "persistence must inject a `replicate` sidecar container"
        );
    }

    #[test]
    fn persistence_mounts_app_db_on_main_and_sidecar() {
        let mut spec = base_spec();
        spec.persistence = Some(persistence_spec());
        let dep = build_persisted_deployment("console", &spec);
        let pod = dep.spec.unwrap().template.spec.unwrap();

        // app-db volume exists on the pod.
        let vols = pod.volumes.expect("volumes must be on pod spec");
        assert!(
            vols.iter().any(|v| v.name == "app-db"),
            "app-db volume must be on the pod"
        );

        let data_dir = "/var/lib/hanzo/console";
        // Mounted at data_dir on the MAIN container (containers[0]).
        let main = &pod.containers[0];
        let main_vms = main.volume_mounts.as_ref().expect("main mounts");
        assert!(
            main_vms
                .iter()
                .any(|m| m.name == "app-db" && m.mount_path == data_dir),
            "main container must mount app-db at the data_dir"
        );
        // Mounted at data_dir on the sidecar.
        let sidecar = pod
            .containers
            .iter()
            .find(|c| c.name == "replicate")
            .expect("replicate sidecar");
        let side_vms = sidecar.volume_mounts.as_ref().expect("sidecar mounts");
        assert!(
            side_vms
                .iter()
                .any(|m| m.name == "app-db" && m.mount_path == data_dir),
            "replicate sidecar must mount app-db at the data_dir"
        );
    }

    #[test]
    fn persistence_configmap_has_bucket_and_endpoint() {
        let p = resolved_persistence("console", &persistence_spec());
        let yml = render_replicate_yml(&p);
        assert!(yml.contains("bucket: console-db"), "must carry the bucket");
        assert!(
            yml.contains("endpoint: http://s3.hanzo.svc:9000"),
            "must carry the http:// endpoint (scheme is load-bearing)"
        );
        assert!(
            yml.contains("force-path-style: true"),
            "must carry force-path-style for SeaweedFS"
        );
        assert!(
            yml.contains("path: /var/lib/hanzo/console/app.db"),
            "single-DB mode must point at the data_dir/db_path file"
        );
    }

    #[test]
    fn persistence_dir_mode_watches_and_omits_restore_init() {
        let mut pspec = persistence_spec();
        pspec.dir_mode = true;
        pspec.db_path = String::new();
        let p = resolved_persistence("console", &pspec);

        // ConfigMap uses dir: + watch: true, NOT a single path:.
        let yml = render_replicate_yml(&p);
        assert!(
            yml.contains("dir: /var/lib/hanzo/console"),
            "dir_mode emits dir:"
        );
        assert!(yml.contains("watch: true"), "dir_mode emits watch: true");
        assert!(
            yml.contains("pattern: \"**/*.db\""),
            "dir_mode emits the glob, quoted (a bare `*` scalar is invalid YAML)"
        );
        assert!(
            !yml.contains("\n    path:"),
            "dir_mode must NOT emit a single path:"
        );

        // No restore init in dir_mode.
        let mut spec = base_spec();
        spec.persistence = Some(pspec);
        let dep = build_persisted_deployment("console", &spec);
        let inits = dep.spec.unwrap().template.spec.unwrap().init_containers;
        let has_restore = inits
            .map(|v| v.iter().any(|c| c.name == "replicate-restore"))
            .unwrap_or(false);
        assert!(
            !has_restore,
            "dir_mode must NOT inject a restore initContainer"
        );
    }

    // ---- HA (zero-downtime SQLite: StatefulSet + per-pod PVC + preStop) ----

    use crate::crd::HaSpec;

    fn ha_spec() -> HaSpec {
        HaSpec {
            enabled: true,
            data_dir: "/var/lib/cloud".to_string(),
            storage: StorageSpec {
                size: "10Gi".to_string(),
                ..Default::default()
            },
            volume_name: String::new(), // resolves to the "data" default
            primary_service: true,
        }
    }

    /// Assemble the StatefulSet exactly as `reconcile_service_inner` does for a
    /// Service with `ha.enabled` (mirrors `build_persisted_deployment`).
    fn build_ha_statefulset(name: &str, spec: &ServiceSpec) -> StatefulSet {
        let h = resolved_ha(spec.ha.as_ref().unwrap());
        let mut main_vms: Vec<crd_types::VolumeMount> = spec.volume_mounts.clone();
        main_vms.push(ha_data_mount(&h));
        let vm_k8s: Vec<_> = main_vms
            .iter()
            .map(crd_types::VolumeMount::to_k8s)
            .collect();
        let mut main = manifests::build_container(
            name,
            &manifests::image_ref(&spec.image.repository, &spec.image.tag),
            &spec.image.pull_policy,
            spec.command.clone(),
            spec.args.clone(),
            spec.env.iter().map(crd_types::EnvVar::to_k8s).collect(),
            vec![],
            vm_k8s,
            manifests::container_ports(&spec.ports),
            None,
            None,
            None,
        );
        if !spec.pre_stop.is_empty() {
            main.lifecycle = Some(manifests::pre_stop_lifecycle(&spec.pre_stop));
        }
        let containers = vec![main];
        let pvc = manifests::build_pvc_template(
            &h.volume_name,
            &h.storage.storage_class_name,
            h.storage.size.as_str(),
        );
        let mut sts = manifests::build_statefulset(
            name,
            "hanzo",
            manifests::standard_labels(name, "", "", &spec.image.tag),
            manifests::selector_labels(name),
            Some(spec.replicas.unwrap_or(1)),
            containers,
            vec![],
            vec![pvc],
            vec![],
            &format!("{}-hs", name),
        );
        if let Some(s_spec) = sts.spec.as_mut() {
            apply_pod_template_extras(&mut s_spec.template, spec, &None);
        }
        sts
    }

    #[test]
    fn ha_renders_statefulset_with_vct_and_mount() {
        let mut spec = base_spec();
        spec.replicas = Some(2);
        spec.ha = Some(ha_spec());
        let sts = build_ha_statefulset("cloud", &spec);
        let s = sts.spec.expect("statefulset spec");
        assert_eq!(s.replicas, Some(2), "HA runs a fixed 2-pod primary+standby");
        assert_eq!(
            s.service_name.as_deref(),
            Some("cloud-hs"),
            "STS must reference the headless Service for pod DNS"
        );
        let vcts = s
            .volume_claim_templates
            .as_ref()
            .expect("per-pod volumeClaimTemplate must be present");
        assert_eq!(vcts.len(), 1);
        assert_eq!(
            vcts[0].metadata.name.as_deref(),
            Some("data"),
            "default per-pod PVC name is `data`"
        );
        let main = &s.template.spec.as_ref().unwrap().containers[0];
        let vms = main.volume_mounts.as_ref().expect("main mounts");
        assert!(
            vms.iter()
                .any(|m| m.name == "data" && m.mount_path == "/var/lib/cloud"),
            "main container must mount its per-pod PVC at data_dir"
        );
    }

    #[test]
    fn ha_main_carries_configurable_pre_stop() {
        // The HA drain hook (checkpoint → final Sync → release lease) must land
        // on the main container verbatim — NOT the default `sleep 5`.
        let mut spec = base_spec();
        spec.ha = Some(ha_spec());
        spec.pre_stop = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "curl -sf -X POST localhost:8000/internal/drain; sleep 5".to_string(),
        ];
        let sts = build_ha_statefulset("cloud", &spec);
        let s = sts.spec.unwrap();
        let main = &s.template.spec.as_ref().unwrap().containers[0];
        let cmd = main
            .lifecycle
            .as_ref()
            .and_then(|l| l.pre_stop.as_ref())
            .and_then(|h| h.exec.as_ref())
            .and_then(|e| e.command.as_ref())
            .expect("configurable preStop exec command");
        assert_eq!(cmd, &spec.pre_stop, "preStop must be the configured command");
    }

    #[test]
    fn default_pre_stop_is_sleep_5_when_unset() {
        // Byte-identical guard: no HA, no configured preStop → the builder's
        // default `sleep 5` drain is injected on the main container.
        let labels = manifests::standard_labels("svc", "", "", "v1");
        let sel = manifests::selector_labels("svc");
        let main = manifests::build_container(
            "svc", "img", "", vec![], vec![], vec![], vec![], vec![], vec![], None, None, None,
        );
        let dep = manifests::build_deployment(
            "svc", "default", labels, sel, Some(1), vec![main], vec![], "", vec![], "",
        );
        let c = &dep.spec.unwrap().template.spec.unwrap().containers[0];
        let cmd = c
            .lifecycle
            .as_ref()
            .and_then(|l| l.pre_stop.as_ref())
            .and_then(|h| h.exec.as_ref())
            .and_then(|e| e.command.as_ref())
            .expect("default preStop");
        assert_eq!(
            cmd,
            &vec!["/bin/sh".to_string(), "-c".to_string(), "sleep 5".to_string()]
        );
    }

    #[test]
    fn primary_service_selects_role_primary() {
        let sel = manifests::selector_labels("cloud");
        let svc = manifests::build_primary_service(
            "cloud-primary",
            "hanzo",
            manifests::standard_labels("cloud", "", "", ""),
            vec![],
            sel,
        );
        let selector = svc.spec.unwrap().selector.expect("primary selector");
        assert_eq!(
            selector.get("hanzo.ai/role").map(String::as_str),
            Some("primary"),
            "primary Service must select only the lease holder"
        );
        assert_eq!(
            selector.get("app.kubernetes.io/name").map(String::as_str),
            Some("cloud"),
            "primary Service must still scope to the app"
        );
    }

    #[test]
    fn ha_suppresses_sidecar_persistence() {
        // HA + persistence set together: HA wins the workload type and the
        // sidecar/restore-init are suppressed (two shippers corrupt the WAL).
        // (Mirrors the controller's `persistence = if ha.is_some() { None }`.)
        let mut spec = base_spec();
        spec.ha = Some(ha_spec());
        spec.persistence = Some(persistence_spec());
        let ha_on = spec.ha.as_ref().filter(|h| h.enabled).is_some();
        let effective_persistence: Option<PersistenceSpec> = if ha_on {
            None
        } else {
            spec.persistence.clone()
        };
        assert!(
            effective_persistence.is_none(),
            "persistence must be suppressed when HA is enabled"
        );
    }
}
