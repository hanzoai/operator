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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::autoscaling::v2::{CrossVersionObjectReference, HorizontalPodAutoscaler};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service as CoreService};
use k8s_openapi::api::networking::v1::{Ingress, NetworkPolicy};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{debug, error, info, warn};

use crate::apply;
use crate::controllers::upgrade::{self, PreflightInputs};
use crate::core::health::{self, BootOutcome};
use crate::core::{OperatorError, Result};
use crate::crd::{
    App, KMSSecretRef, PersistenceSpec, Phase, Service as ServiceCR, ServiceSpec, ServiceStatus,
    UpgradePhase, UpgradePolicySpec, UpgradeRecord, UpgradeStatus,
};
use crate::crd_types;
use crate::manifests;

use super::owner_ref_for;
use crate::crd_types::{build_condition, carry_transition_time, status_changed, Condition};

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
    if p.storage.is_some() {
        crd_types::Volume {
            name: APP_DB_VOLUME.to_string(),
            persistent_volume_claim: Some(crd_types::PersistentVolumeClaimVolumeSource {
                claim_name: app_db_pvc_name(name),
                read_only: None,
            }),
            ..Default::default()
        }
    } else {
        crd_types::Volume {
            name: APP_DB_VOLUME.to_string(),
            empty_dir: Some(crd_types::EmptyDirVolumeSource::default()),
            ..Default::default()
        }
    }
}

/// The pod volume that mounts the `replicate.yml` ConfigMap.
fn replicate_config_volume(name: &str) -> crd_types::Volume {
    crd_types::Volume {
        name: REPLICATE_CONFIG_VOLUME.to_string(),
        config_map: Some(crd_types::ConfigMapVolumeSource {
            name: replicate_config_name(name),
            ..Default::default()
        }),
        ..Default::default()
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

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
    /// Cluster-wide kill switch for the managed-upgrade FSM (env
    /// `UPGRADE_FSM_ENABLED`). Off ⇒ every Service applies `spec.image` directly
    /// (historical behavior) regardless of its `spec.upgradePolicy`.
    pub upgrade_enabled: bool,
    /// Shared leader-election flag. `leader.rs::run()` keeps looping (is_leader=
    /// false) on lease loss rather than returning, so a lease-lost operator would
    /// otherwise keep reconciling and fight the new leader's FSM image-flips. The
    /// Service reconcile re-checks this every reconcile and no-ops when not the
    /// leader (fail-closed split-brain guard, MED-2).
    pub leader_flag: Arc<AtomicBool>,
}

/// Which CR kind holds the claim on one `(namespace, name)` workload.
///
/// `App` and `Service` materialize identical children through
/// [`reconcile_service_inner`] — `app::reconcile` delegates to it — differing
/// only in the ownerRef they stamp. Both apply as the single `hanzo-operator`
/// field manager with `force()`, and server-side apply keys conflict detection
/// on field-manager identity, so two live claims on one name never conflict:
/// they force-flip the Deployment's controller ownerRef on every reconcile
/// instead. App is the canonical workload kind, so Service yields.
///
/// Yielding is what makes a superseded Service CR safe to delete. The ownerRef
/// carries `blockOwnerDeletion` + `controller: true`, so deleting the Service CR
/// while it happens to hold the ownerRef garbage-collects the live Deployment
/// with it. Once App deterministically owns the workload, that delete collects
/// nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Claim {
    /// No same-named App CR: this Service CR is the sole declarer. Materialize.
    Sole,
    /// A same-named App CR declares this workload. Materialize nothing.
    Superseded,
}

/// Decide the claim from an App lookup, where `None` is a failed lookup.
///
/// A failed lookup reads as `Sole`. This guard arbitrates between two live
/// declarers rather than guarding a boundary, so a transient API error degrades
/// to the historical behavior instead of freezing every Service CR: a Service
/// with no App is never superseded, and the tenant fleet — `platform.hanzo.ai`
/// writes `kind: Service` per tenant app — is exactly that set.
pub(crate) fn claim(app_exists: Option<bool>) -> Claim {
    match app_exists {
        Some(true) => Claim::Superseded,
        Some(false) | None => Claim::Sole,
    }
}

/// Look up a same-named App CR. `None` on a failed lookup — see [`claim`].
async fn app_exists(client: &Client, namespace: &str, name: &str) -> Option<bool> {
    let api: Api<App> = Api::namespaced(client.clone(), namespace);
    match api.get_opt(name).await {
        Ok(found) => Some(found.is_some()),
        Err(e) => {
            warn!(error = %e, name, namespace, "App lookup failed; reconciling as sole declarer");
            None
        }
    }
}

/// Status for a superseded Service CR: `Degraded` + a `SupersededByApp` Ready
/// condition naming the App that took the claim. Loud on purpose — the CR is
/// inert and wants deleting, so it must not read `Running`.
fn supersede_status(cr: &ServiceCR, prior: &ServiceStatus) -> ServiceStatus {
    let observed_generation = cr.meta().generation.unwrap_or(0);
    let name = cr.name_any();
    let mut status = ServiceStatus {
        phase: Some(Phase::Degraded),
        observed_generation,
        ..Default::default()
    };
    let mut cond = build_condition(
        "Ready",
        false,
        "SupersededByApp",
        &format!(
            "App/{name} declares this workload; this Service CR materializes nothing. Delete it."
        ),
        observed_generation,
    );
    carry_transition_time(&prior.conditions, &mut cond);
    upsert_condition(&mut status.conditions, cond);
    status
}

/// Reconcile a canonical `Service` CR.
pub async fn reconcile_service(cr: Arc<ServiceCR>, ctx: Arc<Ctx>) -> Result<Action> {
    // Split-brain guard (MED-2): only the lease holder mutates. Controllers are
    // spawned once leadership is first acquired, but `leader.rs::run()` keeps
    // looping with is_leader=false on lease LOSS instead of returning — so a
    // lease-lost operator would keep reconciling and drive the FSM against the
    // new leader (fighting image-flips). Re-check every reconcile and no-op when
    // not the leader; a short requeue picks the CR back up when leadership
    // returns. Fail-closed: a non-leader flips no image and creates no pre-flight.
    if !ctx.leader_flag.load(Ordering::Relaxed) {
        debug!("not leader; skipping Service reconcile (fail-closed split-brain guard)");
        return Ok(Action::requeue(Duration::from_secs(15)));
    }

    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("Service has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "Service");

    // Prior status — keeps condition timestamps stable, skips no-op status
    // writes, and is the persisted input the upgrade FSM resumes from.
    let prior_status = cr.status.clone().unwrap_or_default();

    // One workload, one declarer: yield to a same-named App CR before touching
    // the cluster, so the two kinds never force-flip one Deployment's ownerRef.
    // See `Claim`.
    if claim(app_exists(&ctx.client, &namespace, &name).await) == Claim::Superseded {
        warn!(
            name,
            namespace, "App CR declares this workload; Service CR yields and materializes nothing"
        );
        let status = supersede_status(&cr, &prior_status);
        write_status_if_changed(&ctx, &name, &namespace, &cr, status, &prior_status).await;
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    // Managed-upgrade FSM (opt-in). Decide the image the Deployment must run NOW
    // + the next FSM state, reading the LIVE Deployment first so `running` /
    // `prod_healthy` reflect the current cluster (before this reconcile applies).
    // When the FSM is inactive, `effective_image` is None and the prior upgrade
    // status carries through untouched — historical blind-apply behavior.
    let drive = drive_upgrade_fsm(&ctx, &name, &namespace, &cr, &owner, &prior_status).await?;

    reconcile_service_inner(
        &ctx.client,
        &name,
        &namespace,
        &cr.spec,
        owner.clone(),
        drive.effective_image.as_deref(),
    )
    .await?;

    // Converge the pre-flight resources to the FSM's next phase (create the
    // candidate boot pod + clone when Preflighting, GC them otherwise). Non-fatal.
    if let Some(conv) = &drive.converge {
        if let Err(e) = upgrade::converge_preflight(
            &ctx.client,
            &namespace,
            &name,
            &conv.target,
            conv.want,
            conv.inputs.as_ref(),
        )
        .await
        {
            warn!(error = %e, name, "pre-flight converge failed (non-fatal; retried next reconcile)");
        }
    }

    // Status writeback: poll the Deployment for ready replica count.
    let dep_api: Api<Deployment> = Api::namespaced(ctx.client.clone(), &namespace);
    let dep = dep_api.get_opt(&name).await?;
    let mut status = ServiceStatus {
        observed_generation: cr.meta().generation.unwrap_or(0),
        ..Default::default()
    };
    if let Some(d) = dep {
        if let Some(s) = d.status {
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
    let mut cond = build_condition(
        "Ready",
        ready,
        if ready { "Available" } else { "NotReady" },
        &format!(
            "{}/{} replicas ready",
            status.ready_replicas, desired_replicas
        ),
        status.observed_generation,
    );
    // Only advance lastTransitionTime on a real Ready flip — otherwise the
    // fresh now() timestamp would make every reconcile mutate the CR.
    carry_transition_time(&prior_status.conditions, &mut cond);
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

    // Carry the FSM state onto the status (unchanged when the FSM is inactive).
    status.last_good_image = drive.last_good_image;
    status.upgrade = drive.next_upgrade;
    status.upgrade_history = drive.upgrade_history;

    write_status_if_changed(&ctx, &name, &namespace, &cr, status, &prior_status).await;

    // Requeue faster while an upgrade is actively progressing so the FSM drives
    // pre-flight → roll → success/rollback promptly; otherwise the steady 60s.
    Ok(Action::requeue(Duration::from_secs(drive.requeue_secs)))
}

/// Write `status` to the Service CR, skipping the no-op.
///
/// An unconditional status merge bumps resourceVersion on every reconcile, which
/// the watch re-delivers as an `object updated` event → a self-triggered
/// reconcile storm (~2.75/s/CR across the fleet). Writing only on real change
/// breaks the loop.
async fn write_status_if_changed(
    ctx: &Ctx,
    name: &str,
    namespace: &str,
    cr: &ServiceCR,
    status: ServiceStatus,
    prior: &ServiceStatus,
) {
    if !status_changed(&status, prior) {
        return;
    }
    let api: Api<ServiceCR> = Api::namespaced(ctx.client.clone(), namespace);
    // Optimistic-concurrency precondition (LOW-2): pin the observed
    // resourceVersion so a stale ex-leader writing in the ~10s lease-overlap
    // window 409s here instead of clobbering the live leader's status (a
    // possible unnecessary rollback-of-healthy). The live leader always holds
    // the fresh RV from its watch cache, so its write is unaffected; a 409 is
    // warn-logged and picked up on the next reconcile. Falls back to no
    // precondition only if the object somehow carries no resourceVersion.
    let phase = status.phase.clone();
    let patch = match cr.meta().resource_version.as_deref() {
        Some(rv) => serde_json::json!({"metadata": {"resourceVersion": rv}, "status": status}),
        None => serde_json::json!({"status": status}),
    };
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update Service status (stale resourceVersion 409, or CRD not installed)");
    } else {
        debug!(name, namespace, ?phase, "Service status updated");
    }
}

/// Inject surge co-location affinity iff: the CR opted in (`surgeColocation`),
/// the strategy is a RollingUpdate (anything but `Recreate`; empty defaults to
/// RollingUpdate), AND the pod mounts a real PVC. Pure so the fleet-safety gate
/// is unit-tested without a cluster.
fn should_colocate(surge_colocation: bool, strategy: &str, mounts_pvc: bool) -> bool {
    surge_colocation && strategy != "Recreate" && mounts_pvc
}

/// Public alias for use by compat facades. Facades apply `spec.image` directly
/// (`effective_image = None`); the managed-upgrade FSM is driven only by the
/// canonical `Service` controller's `reconcile_service`.
pub async fn reconcile_service_inner_pub(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &ServiceSpec,
    owner: OwnerReference,
) -> Result<()> {
    reconcile_service_inner(client, name, namespace, spec, owner, None).await
}

/// Shared implementation. Materializes Deployment + Service + Ingress +
/// HPA + PDB + NetworkPolicy + KMSSecret children.
///
/// `effective_image`, when `Some`, overrides the Deployment's main-container
/// image — the managed-upgrade FSM's decision (which may hold the current image
/// during a pre-flight, or the last-good image during a rollback). `None` ⇒ the
/// Deployment runs `spec.image` directly.
async fn reconcile_service_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &ServiceSpec,
    owner: OwnerReference,
    effective_image: Option<&str>,
) -> Result<()> {
    let std_labels =
        manifests::standard_labels(name, &spec.component, &spec.part_of, &spec.image.tag);
    let sel_labels = manifests::selector_labels(name);
    let extra_labels = spec.labels.clone().unwrap_or_default();
    let all_labels = manifests::merge_labels(&[&std_labels, &extra_labels]);

    // Resolve persistence once (defaults filled in) when enabled. Drives the
    // auto-injected app-db mount, ConfigMap, restore init, and sidecar below.
    let persistence = spec
        .persistence
        .as_ref()
        .filter(|p| p.enabled)
        .map(|p| resolved_persistence(name, p));

    // 1. Build the main container honoring spec.env/volumes/volumeMounts.
    let env_k8s: Vec<_> = spec.env.iter().map(crd_types::EnvVar::to_k8s).collect();
    let env_from_k8s: Vec<_> = spec
        .env_from
        .iter()
        .map(crd_types::EnvFromSource::to_k8s)
        .collect();
    // Honor spec.volume_mounts, then auto-inject the shared app-db mount on
    // the MAIN container so the app reads/writes the DB the sidecar streams.
    let mut main_vms: Vec<crd_types::VolumeMount> = spec.volume_mounts.clone();
    if let Some(p) = &persistence {
        main_vms.push(main_app_db_mount(p));
    }
    let vm_k8s: Vec<_> = main_vms
        .iter()
        .map(crd_types::VolumeMount::to_k8s)
        .collect();
    // The upgrade FSM's effective decision overrides the image when set;
    // otherwise the Deployment runs spec.image directly.
    let resolved_image = effective_image
        .map(str::to_string)
        .unwrap_or_else(|| manifests::image_ref(&spec.image.repository, &spec.image.tag));
    let main = manifests::build_container(
        name,
        &resolved_image,
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
            .and_then(manifests::build_probe),
        spec.readiness_probe
            .as_ref()
            .and_then(manifests::build_probe),
    );
    let mut containers = vec![main];
    containers.extend(spec.sidecars.iter().map(crd_types::Container::to_k8s));
    // Auto-inject the replicate sidecar (streams the WAL to SeaweedFS).
    if let Some(p) = &persistence {
        containers.push(replicate_sidecar(p).to_k8s());
    }

    // 2. Build and apply Deployment.
    //
    // When HPA is enabled, the operator MUST NOT own `spec.replicas` — server-
    // side apply would otherwise fight the HPA on every reconcile cycle.
    // Passing `None` here removes the field from the desired state, so the
    // HPA becomes the sole field manager for replicas. The initial scale is
    // then determined by `spec.autoscaling.minReplicas` (the HPA's floor).
    let mut all_volumes: Vec<crd_types::Volume> = spec.volumes.clone();
    if let Some(p) = &persistence {
        // Shared live-DB volume (PVC or emptyDir) + the replicate.yml mount.
        all_volumes.push(app_db_volume(name, p));
        all_volumes.push(replicate_config_volume(name));
    }
    let volumes_k8s: Vec<_> = all_volumes.iter().map(crd_types::Volume::to_k8s).collect();
    // Does the pod mount a real PVC? Computed before volumes_k8s is moved into
    // build_deployment — the precondition for surge co-location.
    let mounts_pvc = volumes_k8s
        .iter()
        .any(|v| v.persistent_volume_claim.is_some());
    let ips_k8s: Vec<_> = spec
        .image_pull_secrets
        .iter()
        .map(crd_types::LocalObjectReference::to_k8s)
        .collect();
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
        if let Some(annotations) = &spec.annotations {
            if let Some(meta) = d_spec.template.metadata.as_mut() {
                meta.annotations = Some(annotations.clone());
            }
        }
        // Zero-downtime surge co-location — OPT-IN (spec.surgeColocation). Only a
        // RollingUpdate service whose data is a single RWO PVC needs it, and only
        // if the store is safe under a brief same-host two-pod overlap (SQLite
        // WAL + busy_timeout). Pin the surge pod to the volume's node so it
        // bind-mounts the already-attached volume instead of dead-locking on
        // Multi-Attach. Exclusive-lock engines opt OUT (they use strategy
        // Recreate), so the affinity is never injected implicitly.
        if should_colocate(spec.surge_colocation, &spec.strategy, mounts_pvc) {
            if let Some(pod) = d_spec.template.spec.as_mut() {
                pod.affinity = Some(manifests::colocation_affinity(&sel_labels));
            }
        }
        // Spec init containers, plus the auto-injected replicate-restore init.
        // dir_mode omits the restore init — directory restore is best-effort
        // via the sidecar's restore-on-boot (a single file path can't address
        // a fan-out of per-org/user DBs).
        let mut inits: Vec<_> = spec
            .init_containers
            .iter()
            .map(crd_types::Container::to_k8s)
            .collect();
        if let Some(p) = &persistence {
            if !p.dir_mode {
                inits.push(replicate_restore_init(p).to_k8s());
            }
        }
        if !inits.is_empty() {
            if let Some(pod) = d_spec.template.spec.as_mut() {
                pod.init_containers = Some(inits);
            }
        }
        // Pod securityContext.fsGroup — opt-in (spec.fsGroup). Lets a non-root
        // image write a persistence PVC (the kubelet chowns the volume to this
        // GID + adds it to every container's supplementary groups).
        if let Some(fsg) = spec.fs_group {
            if let Some(pod) = d_spec.template.spec.as_mut() {
                pod.security_context = Some(k8s_openapi::api::core::v1::PodSecurityContext {
                    fs_group: Some(fsg),
                    ..Default::default()
                });
            }
        }
    }
    set_owner(&mut deploy.metadata.owner_references, &owner);
    let deps: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    // Deployments may carry a stale server-defaulted volume source (an
    // `emptyDir` left from a past source-less apply) or a duplicate probe
    // handler that SSA-merge cannot clear; `apply_or_recreate` deterministically
    // recreates from the desired (single-source) spec in that case. Standalone
    // PVCs re-attach; healthy Deployments apply cleanly and never recreate.
    apply::apply_or_recreate(&deps, &deploy).await?;

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
        apply::apply_configmap(&cms, &cm).await?;
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
        apply::apply_service(&svcs, &svc).await?;
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

    // 5. HPA.
    if let Some(as_spec) = &spec.autoscaling {
        if as_spec.enabled {
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

    debug!(name, namespace, "Service reconciled");
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
pub async fn run_service_controller(
    client: Client,
    namespace: String,
    api_group: String,
    leader_flag: Arc<AtomicBool>,
) {
    let api: Api<ServiceCR> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting Service controller");
    // Cluster-wide kill switch for the managed-upgrade FSM. Default OFF, so the
    // first deploy of this binary is a safe drop-in: Services apply spec.image
    // directly (historical behavior) until the operator is explicitly enabled.
    let upgrade_enabled = std::env::var("UPGRADE_FSM_ENABLED")
        .map(|v| v == "true")
        .unwrap_or(false);
    if upgrade_enabled {
        info!("managed-upgrade FSM enabled (Services with spec.upgradePolicy.enabled roll through pre-flight → health-gate → auto-rollback)");
    }
    let ctx = Arc::new(Ctx {
        client,
        api_group,
        upgrade_enabled,
        leader_flag,
    });
    Controller::new(api, Config::default())
        .run(reconcile_service, on_error_service, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "Service reconcile error");
            }
        })
        .await;
}

// ============================================================================
// Pre-flight orphan GC (MED-1) — the startup + periodic reclaimer.
//
// `converge_preflight` sweeps a Service's OWN pre-flight every reconcile, but a
// disable-sweep that silently failed (`delete_stale_preflight` swallows errors,
// then `upgrade_inactive` clears `status.upgrade` so no later reconcile retries),
// or an operator crash mid-pre-flight before the status was written, leaves the
// clone PVC + VolumeSnapshot (FULL copies of live tenant data) and the
// real-credential candidate pod with NO reconcile that ever retries. Owner-ref GC
// does not help while the Service still exists (upgrade finished, CR stays). This
// loop re-derives orphan-ness from the live Service status, so it is robust to
// ALL orphan sources. The pre-flight resource MECHANICS live in `upgrade`; the
// ORPHAN POLICY (below) lives with the Service controller that owns the status.
// ============================================================================

/// The pre-flight liveness of a Service, as the orphan GC sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PreflightLiveness {
    /// The Service is actively pre-flighting this exact target-image hash.
    InFlight(String),
    /// The Service exists but is not pre-flighting (upgrade done/failed/never),
    /// or the Service is gone — any pre-flight resource for it is an orphan.
    NotInFlight,
    /// The Service status could not be read (transient API error) — fail-closed:
    /// never reclaim on uncertainty.
    Unknown,
}

/// Map a Service status to its pre-flight liveness. A Service is pre-flighting
/// iff `status.upgrade.phase == Preflighting`; its in-flight hash is then
/// `target_hash(upgrade.target_image)` — the SAME hash the pre-flight resources
/// carry in `hanzo.ai/preflight-target`, so a matching resource is kept and a
/// superseded/stale one is reclaimed. Once the roll leaves Preflighting the
/// converge step has already swept the pre-flight, so any that remain are
/// orphans. Pure.
fn liveness_from_status(status: &ServiceStatus) -> PreflightLiveness {
    match status.upgrade.as_ref() {
        Some(u) if u.phase == Some(UpgradePhase::Preflighting) => {
            PreflightLiveness::InFlight(upgrade::target_hash(&u.target_image))
        }
        _ => PreflightLiveness::NotInFlight,
    }
}

/// True when a pre-flight resource carrying `resource_hash` is an orphan given
/// its owning Service's liveness. Fail-closed: `Unknown` ⇒ never an orphan.
/// Pure — the unit of test for the GC decision.
fn is_preflight_orphan(resource_hash: &str, live: &PreflightLiveness) -> bool {
    match live {
        PreflightLiveness::InFlight(h) => resource_hash != h,
        PreflightLiveness::NotInFlight => true,
        PreflightLiveness::Unknown => false,
    }
}

/// True when `created` is within `grace_secs` of `now`. The GC skips a pre-flight
/// resource this young so a periodic sweep never races a reconcile that just
/// created the pre-flight but has not yet written `status=Preflighting` (within
/// one reconcile, converge creates the resources BEFORE the status patch). A
/// missing timestamp is treated as old enough (a live object always has one).
/// Pure.
fn within_grace(created: Option<jiff::Timestamp>, now: jiff::Timestamp, grace_secs: i64) -> bool {
    match created {
        Some(c) => now.duration_since(c).as_secs() < grace_secs,
        None => false,
    }
}

/// Read a Service's live pre-flight liveness. `Ok(None)` (Service gone) ⇒
/// `NotInFlight` (reclaim its residue now rather than wait on owner-ref GC); an
/// API error ⇒ `Unknown` (fail-closed keep).
async fn service_preflight_liveness(
    client: &Client,
    namespace: &str,
    name: &str,
) -> PreflightLiveness {
    let api: Api<ServiceCR> = Api::namespaced(client.clone(), namespace);
    match api.get_opt(name).await {
        Ok(Some(svc)) => liveness_from_status(&svc.status.unwrap_or_default()),
        Ok(None) => PreflightLiveness::NotInFlight,
        Err(e) => {
            warn!(error = %e, namespace, name, "pre-flight GC: Service status read failed; keeping its pre-flight (fail-closed)");
            PreflightLiveness::Unknown
        }
    }
}

/// One GC sweep: list every pre-flight resource in `namespace` (empty ⇒ all) and
/// reclaim each orphan (older than `grace_secs`). One Service lookup per
/// `(namespace, service)` is memoized across its resources. Returns the count
/// reclaimed.
async fn gc_orphan_preflight(client: &Client, namespace: &str, grace_secs: i64) -> usize {
    let now = jiff::Timestamp::now();
    let refs = upgrade::list_preflight(client, namespace).await;
    let mut liveness: std::collections::HashMap<(String, String), PreflightLiveness> =
        std::collections::HashMap::new();
    let mut reclaimed = 0usize;
    for r in &refs {
        if within_grace(r.created, now, grace_secs) {
            continue;
        }
        let key = (r.namespace.clone(), r.of.clone());
        let live = match liveness.get(&key) {
            Some(l) => l.clone(),
            None => {
                let l = service_preflight_liveness(client, &r.namespace, &r.of).await;
                liveness.insert(key, l.clone());
                l
            }
        };
        if is_preflight_orphan(&r.hash, &live) {
            upgrade::delete_preflight(client, r).await;
            info!(
                namespace = %r.namespace, name = %r.name, of = %r.of, kind = ?r.kind,
                "reclaimed orphaned pre-flight resource (MED-1)"
            );
            reclaimed += 1;
        }
    }
    if reclaimed > 0 {
        info!(reclaimed, "pre-flight orphan GC swept");
    }
    reclaimed
}

/// Startup + periodic pre-flight orphan GC loop (MED-1). Sweeps once immediately
/// (startup, before any leak can outlive a restart) then every
/// `PREFLIGHT_GC_INTERVAL_SECS` (default 600, floor 30). Leader-gated
/// (fail-closed: a non-leader never deletes, mirroring `reconcile_service`) and
/// INDEPENDENT of `UPGRADE_FSM_ENABLED`: orphans left by a PRIOR enabled lifetime
/// — including one where the gate was then turned back OFF — must still be
/// reclaimed, and a clone PVC / VolumeSnapshot is a live-tenant-data exposure.
/// Cheap no-op on a cluster that never ran the FSM (the label-filtered lists come
/// back empty).
pub async fn run_preflight_gc(client: Client, namespace: String, leader_flag: Arc<AtomicBool>) {
    let interval_secs = std::env::var("PREFLIGHT_GC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(600)
        .max(30);
    let grace_secs = std::env::var("PREFLIGHT_GC_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(300);
    info!(
        interval_secs,
        grace_secs, "pre-flight orphan GC loop starting (leader-gated)"
    );
    loop {
        if leader_flag.load(Ordering::Relaxed) {
            gc_orphan_preflight(&client, &namespace, grace_secs).await;
        } else {
            debug!("pre-flight GC: not leader; skipping sweep (fail-closed)");
        }
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

// ============================================================================
// Managed-upgrade FSM driver — gathers live observations, runs the pure
// `upgrade::plan`, and assembles the status deltas + pre-flight converge request.
// The decision logic is pure (upgrade.rs); this is the thin imperative shell.
// ============================================================================

/// One reconcile's worth of upgrade drive: the image to run, the next persisted
/// status, and the pre-flight converge request.
struct UpgradeDrive {
    /// `Some` ⇒ override the Deployment image with the FSM's effective decision;
    /// `None` ⇒ the FSM is inactive, use `spec.image`.
    effective_image: Option<String>,
    next_upgrade: Option<UpgradeStatus>,
    last_good_image: Option<String>,
    upgrade_history: Vec<UpgradeRecord>,
    converge: Option<ConvergeReq>,
    requeue_secs: u64,
}

/// What the controller must converge the pre-flight resources toward.
struct ConvergeReq {
    target: String,
    want: bool,
    inputs: Option<PreflightInputs>,
}

/// The inactive-gate path: apply `spec.image` directly (historical blind-apply
/// behavior). If the FSM was mid-flight when it went inactive (the cluster gate
/// flipped off, or the CR disabled its policy), sweep any orphaned pre-flight
/// resources — the clone PVC + VolumeSnapshot (full copies of live tenant data),
/// the candidate pod (real creds), and its egress NetworkPolicy — and clear the
/// now-meaningless in-flight status (MED-4). A never-upgraded Service has no
/// pre-flight status, so it takes the no-op path (no sweep, no LIST).
fn upgrade_inactive(prior: &ServiceStatus) -> UpgradeDrive {
    let had_inflight = prior.upgrade.is_some();
    UpgradeDrive {
        effective_image: None,
        // Clear a frozen in-flight status; the FSM no longer manages it, and it
        // re-derives from scratch if the gate comes back on.
        next_upgrade: None,
        last_good_image: prior.last_good_image.clone(),
        upgrade_history: prior.upgrade_history.clone(),
        converge: had_inflight.then(|| ConvergeReq {
            target: String::new(),
            want: false,
            inputs: None,
        }),
        requeue_secs: 60,
    }
}

/// A refused upgrade (HIGH-1 validation failed): hold production on the current
/// image (NEVER flip to the candidate), create NO pre-flight resources, and sweep
/// any a previously-valid attempt left behind (real creds + a clone of live
/// data). The prior FSM status carries through so fixing the spec resumes it.
fn upgrade_refused(running: Option<&str>, prior: &ServiceStatus) -> UpgradeDrive {
    let hold = running
        .or(prior.last_good_image.as_deref())
        .map(str::to_string);
    UpgradeDrive {
        effective_image: hold,
        next_upgrade: prior.upgrade.clone(),
        last_good_image: prior.last_good_image.clone(),
        upgrade_history: prior.upgrade_history.clone(),
        converge: Some(ConvergeReq {
            target: String::new(),
            want: false,
            inputs: None,
        }),
        requeue_secs: 60,
    }
}

/// True when `spec.volumes` hand-declares the live `<name>-app-db` PVC (the
/// volume the operator auto-injects onto the Deployment for persistence). It must
/// NEVER appear in `spec.volumes`: `build_preflight_inputs` copies `spec.volumes`
/// into the pre-flight pod, so a declared live PVC would be mounted READ-WRITE
/// into the pre-flight instead of the snapshot clone (HIGH-1). Pure over the spec.
fn declares_live_app_db(spec: &ServiceSpec, name: &str) -> bool {
    let live = app_db_pvc_name(name);
    spec.volumes.iter().any(|v| {
        v.persistent_volume_claim
            .as_ref()
            .map(|pvc| pvc.claim_name == live)
            .unwrap_or(false)
    })
}

/// Drive the managed-upgrade FSM one reconcile. Gated on the cluster env AND the
/// per-CR `spec.upgradePolicy.enabled`; inactive ⇒ [`upgrade_inactive`].
async fn drive_upgrade_fsm(
    ctx: &Ctx,
    name: &str,
    namespace: &str,
    cr: &ServiceCR,
    owner: &OwnerReference,
    prior: &ServiceStatus,
) -> Result<UpgradeDrive> {
    let spec = &cr.spec;
    // Gate: cluster kill-switch AND per-CR opt-in.
    let policy = match spec.upgrade_policy.as_ref() {
        Some(p) if ctx.upgrade_enabled && p.enabled => p,
        _ => return Ok(upgrade_inactive(prior)),
    };

    let desired = manifests::image_ref(&spec.image.repository, &spec.image.tag);

    // Read the LIVE Deployment (pre-apply) for the running image + health, so the
    // FSM decides against the CURRENT cluster state.
    let deps: Api<Deployment> = Api::namespaced(ctx.client.clone(), namespace);
    let dep = deps.get_opt(name).await?;
    let running = dep.as_ref().and_then(health::deployment_image);
    let prod_healthy = dep
        .as_ref()
        .map(health::deployment_healthy)
        .unwrap_or(false);

    // Stateful (for the data clone) requires a real durable PVC — persistence
    // enabled AND backed by `storage` (an emptyDir-persistence Service has no
    // `<name>-app-db` PVC to snapshot, so it pre-flights boot-only, not over a
    // clone). Stateful ⇒ pre-flight over a data clone by default; policy overrides.
    let stateful = spec
        .persistence
        .as_ref()
        .is_some_and(|p| p.enabled && p.storage.is_some());
    let cfg = upgrade::Cfg {
        preflight_needed: policy.preflight.unwrap_or(stateful),
        preflight_deadline_secs: policy
            .preflight_deadline_seconds
            .unwrap_or(upgrade::DEFAULT_PREFLIGHT_DEADLINE_SECS),
        rollout_deadline_secs: policy
            .rollout_deadline_seconds
            .unwrap_or(upgrade::DEFAULT_ROLLOUT_DEADLINE_SECS),
        soak_seconds: policy.soak_seconds.unwrap_or(upgrade::DEFAULT_SOAK_SECS),
    };

    // HIGH-1: refuse an unsafe pre-flight BEFORE the FSM engages. A stateful
    // pre-flight boots the real image with the real master key over a clone of
    // live data, so it MUST carry a boot-only marker (`bootEnv`), and
    // `spec.volumes` must never smuggle the live data PVC into the pre-flight.
    // Guard only when an upgrade is actually pending or in flight — never on
    // initial create or steady state (which run no pre-flight): a running
    // Deployment whose desired image is a NEW target (not a revert to last-good),
    // or an already in-flight attempt.
    let upgrade_active = prior.upgrade.is_some()
        || (running.is_some()
            && running.as_deref() != Some(desired.as_str())
            && prior.last_good_image.as_deref() != Some(desired.as_str()));
    if upgrade_active {
        if let Some(reason) = upgrade::upgrade_refusal(
            stateful,
            policy.boot_env.is_empty(),
            declares_live_app_db(spec, name),
        ) {
            warn!(
                name,
                reason,
                "managed upgrade REFUSED — holding production on the current image (no pre-flight created)"
            );
            return Ok(upgrade_refused(running.as_deref(), prior));
        }
    }

    // Observe only what the current phase requires (a pre-flight pod outcome, or
    // whether the rolling candidate pods are crash-looping).
    let cur_phase = prior.upgrade.as_ref().and_then(|u| u.phase.clone());
    let preflight = match (&cur_phase, prior.upgrade.as_ref()) {
        (Some(UpgradePhase::Preflighting), Some(u)) => {
            let pod = if u.preflight_pod.is_empty() {
                upgrade::preflight_pod_name(name, &u.target_image)
            } else {
                u.preflight_pod.clone()
            };
            upgrade::observe_preflight(&ctx.client, namespace, &pod).await
        }
        _ => BootOutcome::Booting,
    };
    let rolling_crashloop = match (&cur_phase, prior.upgrade.as_ref()) {
        (Some(UpgradePhase::Rolling), Some(u)) => {
            pods_crashlooping_for_image(&ctx.client, namespace, name, &u.target_image).await
        }
        _ => false,
    };

    let obs = upgrade::Observed {
        name,
        desired: &desired,
        running: running.as_deref(),
        last_good: prior.last_good_image.as_deref(),
        prod_healthy,
        upgrade: prior.upgrade.as_ref(),
        preflight,
        rolling_crashloop,
        now: jiff::Timestamp::now(),
    };
    let plan = upgrade::plan(&obs, &cfg);

    // Log on a genuine phase transition or a concluded attempt — never every tick.
    let next_phase = plan.next_upgrade.as_ref().and_then(|u| u.phase.clone());
    if next_phase != cur_phase || plan.record.is_some() {
        info!(
            name,
            ?next_phase,
            effective_image = %plan.effective_image,
            reason = plan.reason,
            "upgrade FSM"
        );
    }

    // Assemble the converge request. When holding/entering Preflighting, build
    // the pre-flight inputs for the CURRENT target; otherwise request cleanup.
    let want = matches!(next_phase, Some(UpgradePhase::Preflighting));
    let conv_target = plan
        .next_upgrade
        .as_ref()
        .map(|u| u.target_image.clone())
        .unwrap_or_else(|| desired.clone());
    let inputs = want.then(|| {
        build_preflight_inputs(namespace, name, &conv_target, spec, policy, owner, stateful)
    });

    // last-good: advance only when the FSM proved a value healthy; else carry.
    let last_good_image = plan
        .mark_last_good
        .clone()
        .or_else(|| prior.last_good_image.clone());

    // history: append a concluded record, bounded.
    let mut upgrade_history = prior.upgrade_history.clone();
    if let Some(rec) = plan.record.clone() {
        upgrade::push_history(&mut upgrade_history, rec);
    }

    // Requeue fast while an upgrade is actively progressing.
    let requeue_secs = match next_phase {
        Some(UpgradePhase::Preflighting)
        | Some(UpgradePhase::Rolling)
        | Some(UpgradePhase::RollingBack) => 10,
        _ => 60,
    };

    Ok(UpgradeDrive {
        effective_image: Some(plan.effective_image),
        next_upgrade: plan.next_upgrade,
        last_good_image,
        upgrade_history,
        converge: Some(ConvergeReq {
            target: conv_target,
            want,
            inputs,
        }),
        requeue_secs,
    })
}

/// Assemble the pre-flight candidate-boot inputs from the Service spec. The env
/// is `spec.env` + the boot-only overlay (overlay last ⇒ it wins at runtime), so
/// the candidate runs its real boot path (real KMS master key via `envFrom`)
/// with side effects suppressed. Volumes/mounts are the Service's declared ones;
/// the clone PVC is added by [`upgrade::build_preflight_pod`], never the live PVC.
#[allow(clippy::too_many_arguments)]
fn build_preflight_inputs(
    namespace: &str,
    name: &str,
    candidate_image: &str,
    spec: &ServiceSpec,
    policy: &UpgradePolicySpec,
    owner: &OwnerReference,
    stateful: bool,
) -> PreflightInputs {
    let mut env: Vec<_> = spec.env.iter().map(crd_types::EnvVar::to_k8s).collect();
    env.extend(policy.boot_env.iter().map(crd_types::EnvVar::to_k8s));

    let (data_dir, source_pvc, storage_size, storage_class) = if stateful {
        // `stateful` ⇒ persistence is Some+enabled.
        let p = spec
            .persistence
            .as_ref()
            .expect("stateful ⇒ persistence set");
        let (size, class) = p
            .storage
            .as_ref()
            .map(|s| (s.size.clone(), s.storage_class_name.clone()))
            .unwrap_or_default();
        (Some(p.data_dir.clone()), app_db_pvc_name(name), size, class)
    } else {
        (None, String::new(), String::new(), String::new())
    };

    PreflightInputs {
        namespace: namespace.to_string(),
        candidate_image: candidate_image.to_string(),
        pull_policy: spec.image.pull_policy.clone(),
        command: spec.command.clone(),
        args: spec.args.clone(),
        env,
        env_from: spec
            .env_from
            .iter()
            .map(crd_types::EnvFromSource::to_k8s)
            .collect(),
        volume_mounts: spec
            .volume_mounts
            .iter()
            .map(crd_types::VolumeMount::to_k8s)
            .collect(),
        volumes: spec.volumes.iter().map(crd_types::Volume::to_k8s).collect(),
        readiness_probe: spec
            .readiness_probe
            .as_ref()
            .and_then(manifests::build_probe),
        resources: spec.resources.as_ref().map(manifests::to_k8s_resources),
        image_pull_secrets: spec
            .image_pull_secrets
            .iter()
            .map(crd_types::LocalObjectReference::to_k8s)
            .collect(),
        service_account_name: spec.service_account_name.clone(),
        data_dir,
        source_pvc,
        storage_size,
        storage_class,
        snapshot_class: policy.snapshot_class.clone(),
        // MINIMAL base only — `managed-by` (+ the two `preflight-*` keys `pf_labels`
        // adds). NEVER the Service-selector keys (name/instance) and NEVER the
        // SHARED descriptive keys (component/part-of/version): a pre-flight resource
        // must not be selected by the production Service (HIGH-1) nor by any
        // app-labelled egress-allow policy (HIGH-2), and component/part-of/version
        // serve ZERO function on a throwaway pre-flight pod — they only widen the
        // selector surface. `pf_labels` also strips all five keys as a structural
        // belt, but the honest source is a set that never carried them.
        labels: manifests::managed_by_labels(),
        owner: owner.clone(),
    }
}

/// True when any pod of this Service that runs the candidate `target` image is
/// crash-looping — the fast-rollback signal during a health-gated roll. Filtered
/// to the candidate image so a crashing OLD replica (being torn down) never trips
/// the rollback. Best-effort: a list error is `false` (rely on the deadline).
async fn pods_crashlooping_for_image(
    client: &Client,
    namespace: &str,
    name: &str,
    target: &str,
) -> bool {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let selector = manifests::selector_labels(name)
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    match pods.list(&ListParams::default().labels(&selector)).await {
        Ok(list) => {
            let candidates: Vec<Pod> = list
                .into_iter()
                .filter(|p| pod_runs_image(p, target))
                .collect();
            health::any_pod_crashlooping(&candidates)
        }
        Err(_) => false,
    }
}

/// True when the pod's main container (index 0) runs `target`.
fn pod_runs_image(p: &Pod, target: &str) -> bool {
    p.spec
        .as_ref()
        .and_then(|s| s.containers.first())
        .and_then(|c| c.image.as_deref())
        == Some(target)
}

#[cfg(test)]
mod claim_tests {
    use super::*;
    use crate::crd::ImageSpec;

    /// A Service CR named `name`, carrying `prior` as its persisted status.
    fn service_cr(name: &str, prior: Option<ServiceStatus>) -> ServiceCR {
        let mut cr = ServiceCR::new(
            name,
            ServiceSpec {
                image: ImageSpec {
                    repository: "ghcr.io/hanzoai/test".to_string(),
                    tag: "v1.0.0".to_string(),
                    pull_policy: "IfNotPresent".to_string(),
                },
                replicas: Some(2),
                ..Default::default()
            },
        );
        cr.metadata.namespace = Some("hanzo".to_string());
        cr.metadata.generation = Some(1);
        cr.status = prior;
        cr
    }

    #[test]
    fn a_same_named_app_supersedes_the_service() {
        assert_eq!(claim(Some(true)), Claim::Superseded);
    }

    #[test]
    fn without_an_app_the_service_is_the_sole_declarer() {
        assert_eq!(claim(Some(false)), Claim::Sole);
    }

    /// The tenant fleet (platform.hanzo.ai writes `kind: Service`) has no App
    /// CRs, so a lookup blip must not freeze it — it degrades to sole-declarer.
    #[test]
    fn a_failed_lookup_reconciles_rather_than_freezes() {
        assert_eq!(claim(None), Claim::Sole);
    }

    #[test]
    fn a_superseded_service_reports_degraded_naming_the_app() {
        let cr = service_cr("commerce-admin", None);
        let status = supersede_status(&cr, &ServiceStatus::default());

        assert_eq!(status.phase, Some(Phase::Degraded));
        assert_eq!(status.observed_generation, 1);
        let cond = status
            .conditions
            .iter()
            .find(|c| c.type_ == "Ready")
            .expect("Ready condition");
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, "SupersededByApp");
        assert!(
            cond.message.contains("App/commerce-admin"),
            "message must name the App holding the claim: {}",
            cond.message
        );
    }

    /// A superseded Service must never read `Running` off a stale status: the CR
    /// materializes nothing, and its replica counts describe a Deployment that
    /// the App CR owns.
    #[test]
    fn superseding_clears_a_stale_running_status() {
        let prior = ServiceStatus {
            phase: Some(Phase::Running),
            ready_replicas: 2,
            available_replicas: 2,
            observed_generation: 1,
            ..Default::default()
        };
        let cr = service_cr("commerce-admin", Some(prior.clone()));
        let status = supersede_status(&cr, &prior);

        assert_eq!(status.phase, Some(Phase::Degraded));
        assert_eq!(status.ready_replicas, 0);
        assert_eq!(status.available_replicas, 0);
        assert!(
            status_changed(&status, &prior),
            "the Running→Degraded flip must be written, not skipped as a no-op"
        );
    }

    /// The status write is skipped when nothing changed, so an already-superseded
    /// CR does not bump resourceVersion every 60s (self-triggered reconcile storm).
    #[test]
    fn a_settled_superseded_status_is_a_no_op_write() {
        let cr = service_cr("commerce-admin", None);
        let first = supersede_status(&cr, &ServiceStatus::default());
        let cr = service_cr("commerce-admin", Some(first.clone()));
        let second = supersede_status(&cr, &first);

        assert!(
            !status_changed(&second, &first),
            "a settled supersede must not rewrite status every reconcile"
        );
    }
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
                ..Default::default()
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

    /// Surge co-location gate — the fleet-safety property. OPT-IN + RollingUpdate
    /// + a mounted PVC are ALL required; anything else must NOT get the affinity
    /// (an exclusive-lock engine on Recreate, a non-opted service, or a
    /// volume-less service would only stall or crashloop under it).
    #[test]
    fn colocate_only_when_opted_in_rolling_and_pvc() {
        assert!(should_colocate(true, "RollingUpdate", true));
        assert!(should_colocate(true, "", true)); // empty strategy ⇒ RollingUpdate
        assert!(!should_colocate(false, "RollingUpdate", true)); // not opted in
        assert!(!should_colocate(true, "Recreate", true)); // exclusive-lock default
        assert!(!should_colocate(true, "RollingUpdate", false)); // no PVC to anchor
    }

    /// The co-location affinity shape: SOFT (preferred, never required — a failed
    /// co-location degrades to a fail-safe stalled roll, not an outage), weight
    /// 100, hostname topology, self-selector (matches the app's own pods).
    #[test]
    fn colocation_affinity_is_soft_self_hostname() {
        let mut sel = std::collections::BTreeMap::new();
        sel.insert("app.kubernetes.io/name".to_string(), "iam".to_string());
        let aff = manifests::colocation_affinity(&sel);
        let pa = aff.pod_affinity.unwrap();
        assert!(
            pa.required_during_scheduling_ignored_during_execution
                .is_none(),
            "must be SOFT — never a required (hard) constraint"
        );
        let terms = pa
            .preferred_during_scheduling_ignored_during_execution
            .unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].weight, 100);
        assert_eq!(
            terms[0].pod_affinity_term.topology_key,
            "kubernetes.io/hostname"
        );
        assert_eq!(
            terms[0]
                .pod_affinity_term
                .label_selector
                .as_ref()
                .unwrap()
                .match_labels
                .as_ref()
                .unwrap()
                .get("app.kubernetes.io/name"),
            Some(&"iam".to_string())
        );
    }

    // ---- MED-2: per-reconcile leader fail-closed ----

    /// A non-leader must no-op (fail-closed) — no image flip, no pre-flight, no
    /// cluster mutation. The guard returns BEFORE any spec/cluster work, so a
    /// non-leader reconcile is `Ok` while a leader reconcile proceeds past the
    /// guard (and here errors on the CR's missing namespace, proving the guard is
    /// what short-circuits — not an earlier failure).
    #[tokio::test]
    async fn not_leader_short_circuits_before_any_work() {
        // Building a kube::Client wires the rustls HTTPS connector, which needs a
        // process CryptoProvider (installed in main()); install it here too
        // (idempotent — a repeat call returns Err, ignored).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let client = kube::Client::try_from(kube::Config::new(
            "http://127.0.0.1:1".parse().expect("uri"),
        ))
        .expect("build client");
        let ctx = |leader: bool| {
            Arc::new(Ctx {
                client: client.clone(),
                api_group: "hanzo.ai".to_string(),
                upgrade_enabled: true,
                leader_flag: Arc::new(AtomicBool::new(leader)),
            })
        };
        // ServiceCR::new sets no namespace; the namespace check is AFTER the guard.
        let cr = Arc::new(ServiceCR::new("cloud", base_spec()));

        // Not leader ⇒ short requeue, Ok, zero work.
        assert!(
            reconcile_service(cr.clone(), ctx(false)).await.is_ok(),
            "a non-leader reconcile must no-op (Ok), never reach the FSM/apply"
        );
        // Leader ⇒ proceeds past the guard and fails the namespace check (proving
        // the guard did NOT short-circuit the leader).
        assert!(
            reconcile_service(cr, ctx(true)).await.is_err(),
            "a leader reconcile proceeds past the guard"
        );
    }

    // ---- HIGH-1: live app-db PVC detection ----

    #[test]
    fn declares_live_app_db_detects_a_hand_declared_live_pvc() {
        let mut spec = base_spec();
        // A benign, non-live PVC is fine.
        spec.volumes = vec![crd_types::Volume {
            name: "cache".to_string(),
            persistent_volume_claim: Some(crd_types::PersistentVolumeClaimVolumeSource {
                claim_name: "some-other-pvc".to_string(),
                read_only: None,
            }),
            ..Default::default()
        }];
        assert!(!declares_live_app_db(&spec, "console"));
        // The live `<name>-app-db` PVC smuggled into spec.volumes MUST be caught.
        spec.volumes.push(crd_types::Volume {
            name: "smuggled".to_string(),
            persistent_volume_claim: Some(crd_types::PersistentVolumeClaimVolumeSource {
                claim_name: app_db_pvc_name("console"),
                read_only: None,
            }),
            ..Default::default()
        });
        assert!(
            declares_live_app_db(&spec, "console"),
            "the live <name>-app-db PVC in spec.volumes must be detected (HIGH-1)"
        );
    }

    // ---- HIGH-1: a refused upgrade holds and sweeps ----

    #[test]
    fn upgrade_refused_holds_running_and_sweeps_never_flips() {
        let prior = ServiceStatus {
            last_good_image: Some("img:v1".to_string()),
            ..Default::default()
        };
        let d = upgrade_refused(Some("img:v1"), &prior);
        // Holds the current image — NEVER flips to the candidate (spec.image).
        assert_eq!(d.effective_image.as_deref(), Some("img:v1"));
        // Requests a sweep (want=false) so no pre-flight resources are created/kept.
        let conv = d.converge.expect("a refused upgrade requests a sweep");
        assert!(!conv.want, "refused ⇒ want=false (create nothing, sweep)");
        assert!(conv.inputs.is_none());
    }

    // ---- MED-4: disable-path sweep of orphaned pre-flight resources ----

    #[test]
    fn upgrade_inactive_sweeps_and_clears_a_frozen_inflight_status() {
        // The FSM went inactive (gate off / policy disabled) mid-Preflighting.
        let prior = ServiceStatus {
            upgrade: Some(UpgradeStatus {
                target_image: "img:v2".to_string(),
                phase: Some(UpgradePhase::Preflighting),
                ..Default::default()
            }),
            last_good_image: Some("img:v1".to_string()),
            ..Default::default()
        };
        let d = upgrade_inactive(&prior);
        // The orphaned clone PVC + snapshot + candidate pod + egress policy are swept.
        let conv = d
            .converge
            .expect("a frozen in-flight status requests a sweep");
        assert!(!conv.want, "sweep (want=false), create nothing");
        // The now-meaningless in-flight status is cleared; last-good is preserved.
        assert!(d.next_upgrade.is_none(), "frozen in-flight status cleared");
        assert_eq!(d.last_good_image.as_deref(), Some("img:v1"));
        assert_eq!(
            d.effective_image, None,
            "inactive ⇒ apply spec.image directly"
        );
    }

    #[test]
    fn upgrade_inactive_is_a_noop_when_never_upgraded() {
        // No in-flight status ⇒ nothing to sweep (no LIST), so the fleet-wide
        // inactive path stays cheap.
        let prior = ServiceStatus {
            last_good_image: Some("img:v1".to_string()),
            ..Default::default()
        };
        let d = upgrade_inactive(&prior);
        assert!(
            d.converge.is_none(),
            "no in-flight status ⇒ no sweep / no LIST"
        );
        assert!(d.next_upgrade.is_none());
        assert_eq!(d.effective_image, None);
    }

    // ---------- MED-1: pre-flight orphan GC decision (pure) ----------

    fn preflighting_status(target: &str) -> ServiceStatus {
        ServiceStatus {
            upgrade: Some(UpgradeStatus {
                target_image: target.to_string(),
                phase: Some(UpgradePhase::Preflighting),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn gc_reclaims_a_preflight_whose_service_has_no_inflight_upgrade() {
        // The red-prescribed case: a Service with NO in-flight upgrade (upgrade
        // finished/failed/never — status.upgrade is None) still has a pre-flight
        // pod + clone PVC + VolumeSnapshot left behind (a silently-failed
        // disable-sweep, or a crash before status was written). Every one is an
        // orphan and the startup GC reclaims it.
        let status = ServiceStatus {
            last_good_image: Some("img:v1".to_string()),
            upgrade: None,
            ..Default::default()
        };
        let live = liveness_from_status(&status);
        assert_eq!(live, PreflightLiveness::NotInFlight);
        // Any leftover pre-flight resource (any target hash) is reclaimed.
        assert!(is_preflight_orphan(&upgrade::target_hash("img:v2"), &live));
        assert!(is_preflight_orphan("deadbeef", &live));
    }

    #[test]
    fn gc_keeps_the_current_inflight_preflight_but_reclaims_a_superseded_one() {
        // A Service actively Preflighting `img:v2` keeps exactly that target's
        // pre-flight and reclaims a stale/superseded one (different hash).
        let status = preflighting_status("img:v2");
        let live = liveness_from_status(&status);
        assert_eq!(
            live,
            PreflightLiveness::InFlight(upgrade::target_hash("img:v2"))
        );
        assert!(
            !is_preflight_orphan(&upgrade::target_hash("img:v2"), &live),
            "the current target's pre-flight is NOT an orphan"
        );
        assert!(
            is_preflight_orphan(&upgrade::target_hash("img:v3"), &live),
            "a superseded target's pre-flight IS an orphan"
        );
    }

    #[test]
    fn gc_treats_a_rolling_service_preflight_as_orphaned() {
        // Once the roll leaves Preflighting the converge step has already swept the
        // pre-flight, so any that remain (e.g. after a crash between sweep and
        // status write) are orphans.
        let status = ServiceStatus {
            upgrade: Some(UpgradeStatus {
                target_image: "img:v2".to_string(),
                phase: Some(UpgradePhase::Rolling),
                ..Default::default()
            }),
            ..Default::default()
        };
        let live = liveness_from_status(&status);
        assert_eq!(live, PreflightLiveness::NotInFlight);
        assert!(is_preflight_orphan(&upgrade::target_hash("img:v2"), &live));
    }

    #[test]
    fn gc_fails_closed_on_unknown_service_status() {
        // A transient API error reading the Service ⇒ Unknown ⇒ never reclaim (a
        // clone PVC of live data is worse to delete wrongly than to leak briefly).
        assert!(!is_preflight_orphan(
            &upgrade::target_hash("img:v2"),
            &PreflightLiveness::Unknown
        ));
        assert!(!is_preflight_orphan(
            "anything",
            &PreflightLiveness::Unknown
        ));
    }

    #[test]
    fn gc_grace_window_skips_a_freshly_created_preflight() {
        // A resource younger than the grace window is skipped so the periodic GC
        // never races a reconcile that just created the pre-flight but has not yet
        // persisted status=Preflighting.
        let now = jiff::Timestamp::now();
        let fresh = now
            .checked_sub(jiff::SignedDuration::from_secs(10))
            .unwrap();
        let old = now
            .checked_sub(jiff::SignedDuration::from_secs(600))
            .unwrap();
        assert!(within_grace(Some(fresh), now, 300), "10s < 300s ⇒ skip");
        assert!(!within_grace(Some(old), now, 300), "600s > 300s ⇒ eligible");
        assert!(
            !within_grace(None, now, 300),
            "no timestamp ⇒ not within grace (eligible)"
        );
    }
}
