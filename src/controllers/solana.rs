//! SolanaRuntime reconciler — an agave validator.
//!
//! Unlike bitcoind and geth, the validator takes no config file: everything is
//! a command-line flag, and the flags depend on each other — voting requires a
//! vote account, snapshot tuning is meaningless when snapshot fetch is off. So
//! the operator renders a startup script rather than assembling an args array,
//! which keeps the whole invocation in one readable artifact that can be looked
//! at inside the pod when something is wrong.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{ConfigMap, Probe, Service as CoreService, VolumeMount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{
    Phase, ProbeSpec, ServicePort as CrServicePort, SolanaCluster, SolanaRuntime,
    SolanaRuntimeSpec, SolanaRuntimeStatus,
};
use crate::crd_types::build_condition;
use crate::manifests;

use super::chain::{
    conditions_equivalent, config_volume, local_refs, one_file, secret_volume, tcp_probe,
};
use super::owner_ref_for;

const LEDGER_DIR: &str = "/data/solana/ledger";
const IDENTITY_DIR: &str = "/etc/solana/identity";
const VOTE_DIR: &str = "/etc/solana/vote";
const SCRIPT_DIR: &str = "/etc/solana/script";
const SCRIPT: &str = "run-validator.sh";

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

pub fn rpc_port(spec: &SolanaRuntimeSpec) -> i32 {
    if spec.rpc_port != 0 {
        spec.rpc_port
    } else {
        8899
    }
}

pub fn gossip_port(spec: &SolanaRuntimeSpec) -> i32 {
    if spec.gossip_port != 0 {
        spec.gossip_port
    } else {
        8001
    }
}

/// Whether this validator has an account to vote with. A validator without one
/// still follows the cluster and serves reads; it just has no say.
pub fn votes(spec: &SolanaRuntimeSpec) -> bool {
    spec.vote_account
        .as_ref()
        .is_some_and(|v| !v.name.is_empty())
}

/// Where the validator first reaches the cluster.
///
/// The public clusters have well-known entrypoints, so a CR naming a cluster
/// need not repeat them; anything the CR adds is appended, deduplicated. A
/// custom cluster has no well-known set, so there the CR's list is all there is
/// — and if it is empty the validator has nowhere to start, which `check`
/// refuses rather than deploying a node that gossips into nothing.
pub fn entry_points(spec: &SolanaRuntimeSpec) -> Vec<String> {
    let base: &[&str] = match spec.cluster {
        SolanaCluster::MainnetBeta => &[
            "entrypoint.mainnet-beta.solana.com:8001",
            "entrypoint2.mainnet-beta.solana.com:8001",
        ],
        SolanaCluster::Testnet => &["entrypoint.testnet.solana.com:8001"],
        SolanaCluster::Devnet => &["entrypoint.devnet.solana.com:8001"],
        SolanaCluster::Custom => &[],
    };
    let mut out: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    for e in &spec.entry_points {
        if !out.contains(e) {
            out.push(e.clone());
        }
    }
    out
}

/// A validator needs somewhere to start. On a custom cluster there is no
/// well-known entrypoint to fall back on, so an empty list means the node comes
/// up, gossips to nobody, and never syncs — with a healthy-looking pod.
fn check(spec: &SolanaRuntimeSpec) -> Result<()> {
    if spec.identity_keypair.name.is_empty() {
        return Err(OperatorError::Config(
            "identityKeypair names the validator's own key; it has no identity without one".into(),
        ));
    }
    if entry_points(spec).is_empty() {
        return Err(OperatorError::Config(
            "a custom cluster has no well-known entrypoint, so entryPoints must name at least one"
                .into(),
        ));
    }
    Ok(())
}

/// Render the startup script. Deterministic for a stable spec.
pub fn render_startup(spec: &SolanaRuntimeSpec) -> String {
    let mut b = String::from("#!/bin/sh\n# Rendered by the operator — do not edit\nset -e\n");
    b.push_str("exec agave-validator \\\n");
    b.push_str(&format!("  --identity {IDENTITY_DIR}/identity.json \\\n"));
    b.push_str(&format!("  --ledger {LEDGER_DIR} \\\n"));
    b.push_str(&format!("  --gossip-port {} \\\n", gossip_port(spec)));
    for e in entry_points(spec) {
        b.push_str(&format!("  --entrypoint {e} \\\n"));
    }
    if votes(spec) {
        b.push_str(&format!("  --vote-account {VOTE_DIR}/vote-account.json \\\n"));
    } else {
        b.push_str("  --no-voting \\\n");
    }
    if spec.rpc_enabled {
        b.push_str(&format!("  --rpc-port {} \\\n", rpc_port(spec)));
        b.push_str("  --full-rpc-api \\\n");
    }
    // Replaying from genesis takes longer than the cluster takes to produce the
    // slots, so fetching is the norm and opting out is the deliberate act.
    if !spec.snapshot.as_ref().is_some_and(|s| s.fetch) {
        b.push_str("  --no-snapshot-fetch \\\n");
    }
    // Interval is how often this validator WRITES a snapshot, which it does
    // whether or not it fetched one to start — so the tuning is independent of
    // the fetch decision above and not nested under it.
    if let Some(s) = &spec.snapshot {
        if s.interval_slots > 0 {
            b.push_str(&format!(
                "  --full-snapshot-interval-slots {} \\\n",
                s.interval_slots
            ));
        }
        if s.minimum_download_speed_mbps > 0 {
            b.push_str(&format!(
                "  --minimal-snapshot-download-speed {} \\\n",
                s.minimum_download_speed_mbps
            ));
        }
    }
    for a in &spec.extra_args {
        b.push_str(&format!("  {a} \\\n"));
    }
    // Every line above ends in a continuation, so the command needs a final
    // line that is not one.
    b.push_str("  --log -\n");
    b
}

/// With RPC on, the validator answers `getHealth` and that is the real signal.
/// With it off there is no endpoint to ask, so an open gossip port is the most
/// that can be checked.
fn readiness(spec: &SolanaRuntimeSpec) -> Option<Probe> {
    if spec.rpc_enabled {
        manifests::build_probe(&ProbeSpec {
            path: "/health".to_string(),
            port: rpc_port(spec),
            initial_delay_seconds: 60,
            period_seconds: 30,
            ..Default::default()
        })
    } else {
        Some(tcp_probe(gossip_port(spec), 60, 30))
    }
}

pub async fn reconcile(cr: Arc<SolanaRuntime>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("SolanaRuntime has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "SolanaRuntime");
    reconcile_inner(&ctx.client, &name, &namespace, &cr.spec, owner).await?;
    write_status(&ctx.client, &name, &namespace, &cr).await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

async fn reconcile_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &SolanaRuntimeSpec,
    owner: OwnerReference,
) -> Result<()> {
    check(spec)?;

    let labels =
        manifests::standard_labels(name, "solana-validator", "solana", &spec.node_image.tag);
    let sel = manifests::selector_labels(name);

    let script_name = format!("{name}-startup");
    let mut cm = manifests::build_configmap(
        &script_name,
        namespace,
        labels.clone(),
        one_file(SCRIPT, render_startup(spec)),
    );
    cm.metadata.owner_references = Some(vec![owner.clone()]);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    apply::apply_configmap(&cms, &cm).await?;

    let mut ports = vec![CrServicePort {
        name: "gossip".to_string(),
        container_port: gossip_port(spec),
        service_port: None,
        protocol: "TCP".to_string(),
    }];
    if spec.rpc_enabled {
        ports.push(CrServicePort {
            name: "rpc".to_string(),
            container_port: rpc_port(spec),
            service_port: None,
            protocol: "TCP".to_string(),
        });
    }

    let mut mounts = vec![
        VolumeMount {
            name: "ledger".to_string(),
            mount_path: LEDGER_DIR.to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "identity".to_string(),
            mount_path: IDENTITY_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "script".to_string(),
            mount_path: SCRIPT_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
    ];
    let mut volumes = vec![
        secret_volume("identity", &spec.identity_keypair.name),
        config_volume("script", &script_name),
    ];
    if votes(spec) {
        mounts.push(VolumeMount {
            name: "vote".to_string(),
            mount_path: VOTE_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        });
        volumes.push(secret_volume(
            "vote",
            &spec.vote_account.as_ref().expect("votes() checked it").name,
        ));
    }

    let main = manifests::build_container(
        "validator",
        &manifests::image_ref(&spec.node_image.repository, &spec.node_image.tag),
        &spec.node_image.pull_policy,
        vec!["/bin/sh".to_string(), format!("{SCRIPT_DIR}/{SCRIPT}")],
        vec![],
        vec![],
        vec![],
        mounts,
        manifests::container_ports(&ports),
        spec.resources.as_ref().map(manifests::to_k8s_resources),
        None,
        readiness(spec),
    );

    let pvc =
        manifests::build_pvc_template("ledger", &spec.ledger.storage_class_name, &spec.ledger.size);
    let headless = format!("{name}-headless");
    let mut sts = manifests::build_statefulset(
        name,
        namespace,
        labels.clone(),
        sel.clone(),
        Some(1),
        vec![main],
        volumes,
        vec![pvc],
        local_refs(&spec.image_pull_secrets),
        &headless,
    );
    sts.metadata.owner_references = Some(vec![owner.clone()]);
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    apply::apply(&stss, &sts).await?;

    let svc_ports = manifests::service_ports(&ports);
    let svcs: Api<CoreService> = Api::namespaced(client.clone(), namespace);
    let mut hs = manifests::build_headless_service(
        &headless,
        namespace,
        labels.clone(),
        svc_ports.clone(),
        sel.clone(),
    );
    hs.metadata.owner_references = Some(vec![owner.clone()]);
    apply::apply_service(&svcs, &hs).await?;

    let mut clip = manifests::build_service(name, namespace, labels, svc_ports, sel);
    clip.metadata.owner_references = Some(vec![owner]);
    apply::apply_service(&svcs, &clip).await?;

    info!(name, namespace, cluster = ?spec.cluster, voting = votes(spec), "SolanaRuntime reconciled");
    Ok(())
}

async fn compute_status(
    client: &Client,
    name: &str,
    namespace: &str,
    cr: &SolanaRuntime,
) -> SolanaRuntimeStatus {
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    let ready = stss
        .get_opt(name)
        .await
        .ok()
        .flatten()
        .and_then(|sts| sts.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0)
        >= 1;
    let generation = cr.meta().generation.unwrap_or(0);
    let mut status = SolanaRuntimeStatus {
        phase: Some(if ready { Phase::Running } else { Phase::Creating }),
        ready,
        // Slots come from the validator, and this pass does not ask it.
        slot_height: -1,
        root_slot: -1,
        vote_slot: -1,
        observed_generation: generation,
        ..Default::default()
    };
    status.conditions.push(build_condition(
        "Ready",
        ready,
        if ready { "Available" } else { "NotReady" },
        if ready {
            "validator is running"
        } else {
            "validator is not ready"
        },
        generation,
    ));
    status
}

async fn write_status(client: &Client, name: &str, namespace: &str, cr: &SolanaRuntime) {
    let next = compute_status(client, name, namespace, cr).await;
    if let Some(current) = &cr.status {
        if current.phase == next.phase
            && current.ready == next.ready
            && current.slot_height == next.slot_height
            && current.root_slot == next.root_slot
            && current.vote_slot == next.vote_slot
            && current.is_voting == next.is_voting
            && current.observed_generation == next.observed_generation
            && conditions_equivalent(&current.conditions, &next.conditions)
        {
            return;
        }
    }
    let api: Api<SolanaRuntime> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({ "status": next });
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update SolanaRuntime status");
    }
}

pub fn on_error(_obj: Arc<SolanaRuntime>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "SolanaRuntime reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_solana_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<SolanaRuntime> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting SolanaRuntime controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "SolanaRuntime reconcile error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{ImageSpec, SecretRef, SolanaSnapshotSpec, StorageSpec};

    fn spec() -> SolanaRuntimeSpec {
        SolanaRuntimeSpec {
            node_image: ImageSpec {
                repository: "ghcr.io/hanzoai/agave".to_string(),
                tag: "v2.1.0".to_string(),
                pull_policy: "IfNotPresent".to_string(),
            },
            cluster: SolanaCluster::MainnetBeta,
            identity_keypair: SecretRef {
                name: "validator-identity".to_string(),
                key: String::new(),
            },
            ledger: StorageSpec {
                size: "4Ti".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// A public cluster supplies its own entrypoints so a CR need not repeat
    /// them, and anything the CR adds is appended without duplicating what is
    /// already there.
    #[test]
    fn public_clusters_bring_their_own_entrypoints() {
        assert_eq!(entry_points(&spec()).len(), 2);
        let mut t = spec();
        t.cluster = SolanaCluster::Testnet;
        assert_eq!(entry_points(&t), ["entrypoint.testnet.solana.com:8001"]);

        let mut extra = spec();
        extra.entry_points = vec![
            "entrypoint.mainnet-beta.solana.com:8001".into(), // already known
            "my.entrypoint:8001".into(),
        ];
        let got = entry_points(&extra);
        assert_eq!(got.len(), 3, "the known one is not repeated: {got:?}");
        assert_eq!(got.last().unwrap(), "my.entrypoint:8001");
    }

    /// A custom cluster has no well-known entrypoint to fall back on. Without
    /// one the validator starts, gossips to nobody and never syncs, while the
    /// pod reports healthy — so it is refused.
    #[test]
    fn a_custom_cluster_with_nowhere_to_start_is_refused() {
        let mut s = spec();
        s.cluster = SolanaCluster::Custom;
        assert!(entry_points(&s).is_empty());
        assert!(check(&s).is_err());

        s.entry_points = vec!["private.entrypoint:8001".into()];
        assert!(check(&s).is_ok());

        let mut no_id = spec();
        no_id.identity_keypair.name = String::new();
        assert!(check(&no_id).is_err());
    }

    /// Voting needs an account. Without one the validator follows the cluster
    /// and serves reads but has no say, which is a real configuration and must
    /// be passed explicitly rather than left to a default.
    #[test]
    fn a_validator_without_a_vote_account_says_so() {
        let s = spec();
        assert!(!votes(&s));
        let script = render_startup(&s);
        assert!(script.contains("--no-voting"), "{script}");
        assert!(!script.contains("--vote-account"), "{script}");

        let mut v = spec();
        v.vote_account = Some(SecretRef {
            name: "vote".to_string(),
            key: String::new(),
        });
        assert!(votes(&v));
        let script = render_startup(&v);
        assert!(script.contains("--vote-account"), "{script}");
        assert!(!script.contains("--no-voting"), "{script}");
    }

    /// Interval is how often the validator WRITES a snapshot, which it does
    /// whether or not it fetched one to start. Nesting the tuning under the
    /// fetch decision would silently drop it for any node that opted out of
    /// fetching.
    #[test]
    fn snapshot_tuning_survives_opting_out_of_fetching() {
        let mut s = spec();
        s.snapshot = Some(SolanaSnapshotSpec {
            fetch: false,
            interval_slots: 25000,
            minimum_download_speed_mbps: 0,
        });
        let script = render_startup(&s);
        assert!(script.contains("--no-snapshot-fetch"), "{script}");
        assert!(
            script.contains("--full-snapshot-interval-slots 25000"),
            "{script}"
        );

        // Absent snapshot means no fetch at all, and nothing to tune.
        let bare = render_startup(&spec());
        assert!(bare.contains("--no-snapshot-fetch"), "{bare}");
        assert!(!bare.contains("interval-slots"), "{bare}");
    }

    /// Every line of the invocation is backslash-continued, so the script must
    /// end on a line that is not — otherwise the shell swallows whatever
    /// follows, or the command is left dangling.
    #[test]
    fn the_script_is_a_complete_command() {
        let mut s = spec();
        s.rpc_enabled = true;
        s.extra_args = vec!["--limit-ledger-size".into()];
        let script = render_startup(&s);
        assert!(script.starts_with("#!/bin/sh\n"), "{script}");
        let last = script.lines().last().unwrap();
        assert!(!last.ends_with('\\'), "dangling continuation: {last:?}");
        assert_eq!(last, "  --log -");
        for line in script.lines().skip(4) {
            if line != "  --log -" {
                assert!(line.ends_with('\\'), "line breaks the command: {line:?}");
            }
        }
        assert!(script.contains("--rpc-port 8899"), "{script}");
        assert!(script.contains("--limit-ledger-size"), "{script}");
        assert_eq!(script, render_startup(&s), "stable for a stable spec");
    }
}
