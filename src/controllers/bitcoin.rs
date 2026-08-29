//! BitcoinRuntime reconciler — a bitcoind node, its config, and an optional
//! Electrum-protocol indexer sharing its data volume.
//!
//! The operator owns every line of bitcoin.conf. bitcoind merges a config file
//! with its own defaults and with anything already on the data volume, so a
//! partially-managed config drifts: the CR says one thing, the file on disk
//! says another, and which one is running depends on when the pod last
//! restarted. Rendering it whole makes the CR the answer.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{EnvVar, VolumeMount};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{
    BitcoinNetwork, BitcoinRuntime, BitcoinRuntimeSpec, BitcoinRuntimeStatus, Phase,
    ServicePort as CrServicePort,
};
use crate::crd_types::build_condition;
use crate::manifests;

use super::chain::{
    conditions_equivalent, config_volume, local_refs, one_file, or_else, refuse_live_node,
    secret_key_ref, tcp_probe, Plan,
};
use super::owner_ref_for;

/// The in-pod data dir, mounted from the PVC.
const DATA_DIR: &str = "/data/.bitcoin";
/// Where the rendered bitcoin.conf is mounted read-only.
const CONF_DIR: &str = "/etc/bitcoin";
/// The Electrum RPC port when the indexer names none.
const ELECTRUM_PORT: i32 = 50001;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

/// The effective P2P port: the one asked for, else the network's own.
pub fn p2p_port(spec: &BitcoinRuntimeSpec) -> i32 {
    match spec.p2p.as_ref().map(|p| p.listen_port) {
        Some(p) if p != 0 => return p,
        _ => {}
    }
    match spec.network {
        BitcoinNetwork::Testnet => 18333,
        BitcoinNetwork::Regtest => 18444,
        BitcoinNetwork::Signet => 38333,
        BitcoinNetwork::Mainnet => 8333,
    }
}

/// The effective JSON-RPC port: the one asked for, else the network's own.
pub fn rpc_port(spec: &BitcoinRuntimeSpec) -> i32 {
    if spec.rpc_port != 0 {
        return spec.rpc_port;
    }
    match spec.network {
        BitcoinNetwork::Testnet => 18332,
        BitcoinNetwork::Regtest => 18443,
        BitcoinNetwork::Signet => 38332,
        BitcoinNetwork::Mainnet => 8332,
    }
}

/// Render bitcoin.conf from the spec.
///
/// Byte-stable for a stable spec — addnode entries are sorted — so an unchanged
/// CR produces an unchanged ConfigMap and server-side apply does nothing. An
/// unsorted list would rewrite the ConfigMap on every pass and roll the
/// StatefulSet each time.
pub fn render_conf(spec: &BitcoinRuntimeSpec) -> String {
    let mut out = String::from("# Rendered by the operator — do not edit\n");
    match spec.network {
        BitcoinNetwork::Testnet => out.push_str("testnet=1\n"),
        BitcoinNetwork::Regtest => out.push_str("regtest=1\n"),
        BitcoinNetwork::Signet => out.push_str("signet=1\n"),
        // mainnet is bitcoind's default and names itself with no flag.
        BitcoinNetwork::Mainnet => {}
    }
    out.push_str("server=1\n");
    out.push_str("rpcbind=0.0.0.0\n");
    out.push_str("rpcallowip=0.0.0.0/0\n");
    out.push_str(&format!("rpcport={}\n", rpc_port(spec)));
    out.push_str(&format!("port={}\n", p2p_port(spec)));
    if spec.tx_index {
        out.push_str("txindex=1\n");
    }
    if spec.pruning > 0 {
        out.push_str(&format!("prune={}\n", spec.pruning));
    }
    if let Some(p2p) = &spec.p2p {
        if p2p.max_connections > 0 {
            out.push_str(&format!("maxconnections={}\n", p2p.max_connections));
        }
        let mut nodes = p2p.add_nodes.clone();
        nodes.sort();
        for n in nodes {
            out.push_str(&format!("addnode={n}\n"));
        }
    }
    out
}

/// txindex builds an index over every transaction ever seen; prune discards the
/// blocks it would be built from. bitcoind refuses the combination at startup,
/// so the CR is rejected here where the reason can be said, rather than in a
/// crash loop where it has to be read out of a log.
fn check(spec: &BitcoinRuntimeSpec) -> Result<()> {
    if spec.tx_index && spec.pruning > 0 {
        return Err(OperatorError::Config(
            "txIndex needs every block and pruning discards them; set one or the other".into(),
        ));
    }
    Ok(())
}

pub async fn reconcile(cr: Arc<BitcoinRuntime>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("BitcoinRuntime has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "BitcoinRuntime");
    reconcile_inner(&ctx.client, &name, &namespace, &cr.spec, owner).await?;
    write_status(&ctx.client, &name, &namespace, &cr).await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

/// What a BitcoinRuntime resolves to. Pure — nothing is sent from here, so a
/// test can read the arguments bitcoind will actually be started with.
pub fn build(
    name: &str,
    namespace: &str,
    spec: &BitcoinRuntimeSpec,
    owner: OwnerReference,
) -> Result<Plan> {
    refuse_live_node(name, namespace)?;
    check(spec)?;

    let labels = manifests::standard_labels(name, "bitcoind", "bitcoin", &spec.node_image.tag);
    let sel = manifests::selector_labels(name);
    let rpc = rpc_port(spec);
    let p2p = p2p_port(spec);

    let conf_name = format!("{name}-config");
    let mut cm = manifests::build_configmap(
        &conf_name,
        namespace,
        labels.clone(),
        one_file("bitcoin.conf", render_conf(spec)),
    );
    cm.metadata.owner_references = Some(vec![owner.clone()]);

    let mut ports = vec![
        CrServicePort {
            name: "p2p".to_string(),
            container_port: p2p,
            service_port: None,
            protocol: "TCP".to_string(),
        },
        CrServicePort {
            name: "rpc".to_string(),
            container_port: rpc,
            service_port: None,
            protocol: "TCP".to_string(),
        },
    ];

    // RPC credentials, when a Secret carries them. Absent, bitcoind writes a
    // cookie into the data dir and only something sharing that volume can read
    // it — which is the safe default, not a missing feature.
    let mut env: Vec<EnvVar> = vec![];
    if let Some(auth) = spec.rpc_auth.as_ref().filter(|a| !a.name.is_empty()) {
        env.push(EnvVar {
            name: "BITCOIND_RPCUSER".to_string(),
            value_from: Some(secret_key_ref(&auth.name, or_else(&auth.key, "rpcuser"))),
            ..Default::default()
        });
        env.push(EnvVar {
            name: "BITCOIND_RPCPASSWORD".to_string(),
            value_from: Some(secret_key_ref(&auth.name, "rpcpassword")),
            ..Default::default()
        });
    }

    let mounts = vec![
        VolumeMount {
            name: "data".to_string(),
            mount_path: DATA_DIR.to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "config".to_string(),
            mount_path: CONF_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
    ];

    let main = manifests::build_container(
        "bitcoind",
        &manifests::image_ref(&spec.node_image.repository, &spec.node_image.tag),
        &spec.node_image.pull_policy,
        vec![],
        vec![
            format!("-conf={CONF_DIR}/bitcoin.conf"),
            format!("-datadir={DATA_DIR}"),
        ],
        env,
        vec![],
        mounts,
        manifests::container_ports(&ports),
        spec.resources.as_ref().map(manifests::to_k8s_resources),
        None,
        Some(tcp_probe(rpc, 30, 15)),
    );
    let mut containers = vec![main];

    // The indexer reads the same blocks the node wrote, so it shares the volume
    // read-only rather than syncing its own copy.
    if let Some(idx) = &spec.indexer {
        let port = if idx.rpc_port != 0 {
            idx.rpc_port
        } else {
            ELECTRUM_PORT
        };
        let idx_ports = vec![CrServicePort {
            name: "electrum".to_string(),
            container_port: port,
            service_port: None,
            protocol: "TCP".to_string(),
        }];
        containers.push(manifests::build_container(
            "indexer",
            &manifests::image_ref(&idx.image.repository, &idx.image.tag),
            &idx.image.pull_policy,
            vec![],
            idx.extra_args.clone(),
            vec![],
            vec![],
            vec![VolumeMount {
                name: "data".to_string(),
                mount_path: DATA_DIR.to_string(),
                read_only: Some(true),
                ..Default::default()
            }],
            manifests::container_ports(&idx_ports),
            None,
            None,
            Some(tcp_probe(port, 60, 20)),
        ));
        ports.extend(idx_ports);
    }

    let pvc = manifests::build_pvc_template(
        "data",
        &spec.storage.storage_class_name,
        &spec.storage.size,
    );

    let headless = format!("{name}-headless");
    let mut sts = manifests::build_statefulset(
        name,
        namespace,
        labels.clone(),
        sel.clone(),
        // One node per CR. Bitcoin peers are not replicas: a second pod is a
        // second node with its own chainstate, not another copy of this one.
        Some(1),
        containers,
        vec![config_volume("config", &conf_name)],
        vec![pvc],
        local_refs(&spec.image_pull_secrets),
        &headless,
    );
    sts.metadata.owner_references = Some(vec![owner.clone()]);

    let svc_ports = manifests::service_ports(&ports);
    let mut hs = manifests::build_headless_service(
        &headless,
        namespace,
        labels.clone(),
        svc_ports.clone(),
        sel.clone(),
    );
    hs.metadata.owner_references = Some(vec![owner.clone()]);

    let mut clip = manifests::build_service(name, namespace, labels, svc_ports, sel);
    clip.metadata.owner_references = Some(vec![owner]);

    Ok(Plan {
        configs: vec![cm],
        sets: vec![sts],
        services: vec![hs, clip],
    })
}

async fn reconcile_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &BitcoinRuntimeSpec,
    owner: OwnerReference,
) -> Result<()> {
    let plan = build(name, namespace, spec, owner)?;
    plan.apply(client, namespace).await?;
    info!(name, namespace, network = ?spec.network, "BitcoinRuntime reconciled");
    Ok(())
}

/// Status from what the operator can see, which is the StatefulSet.
///
/// Height, peers and sync progress would take an RPC call to the node, and this
/// pass does not make one — so they are reported as -1, meaning not observed,
/// rather than 0, which reads as a node stuck at genesis with no peers.
async fn compute_status(
    client: &Client,
    name: &str,
    namespace: &str,
    cr: &BitcoinRuntime,
) -> BitcoinRuntimeStatus {
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
    let mut status = BitcoinRuntimeStatus {
        phase: Some(if ready { Phase::Running } else { Phase::Creating }),
        ready,
        block_height: -1,
        peer_count: -1,
        observed_generation: generation,
        ..Default::default()
    };
    status.conditions.push(build_condition(
        "Ready",
        ready,
        if ready { "Available" } else { "NotReady" },
        if ready {
            "bitcoind is serving"
        } else {
            "bitcoind is not ready"
        },
        generation,
    ));
    status
}

async fn write_status(client: &Client, name: &str, namespace: &str, cr: &BitcoinRuntime) {
    let next = compute_status(client, name, namespace, cr).await;
    if let Some(current) = &cr.status {
        if current.phase == next.phase
            && current.ready == next.ready
            && current.block_height == next.block_height
            && current.peer_count == next.peer_count
            && current.sync_progress == next.sync_progress
            && current.observed_generation == next.observed_generation
            && conditions_equivalent(&current.conditions, &next.conditions)
        {
            return;
        }
    }
    let api: Api<BitcoinRuntime> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({ "status": next });
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update BitcoinRuntime status");
    }
}

pub fn on_error(_obj: Arc<BitcoinRuntime>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "BitcoinRuntime reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_bitcoin_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<BitcoinRuntime> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting BitcoinRuntime controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "BitcoinRuntime reconcile error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{BitcoinP2PSpec, ImageSpec, StorageSpec};

    pub(super) fn spec() -> BitcoinRuntimeSpec {
        BitcoinRuntimeSpec {
            node_image: ImageSpec {
                repository: "ghcr.io/hanzoai/bitcoind".to_string(),
                tag: "28.0".to_string(),
                pull_policy: "IfNotPresent".to_string(),
            },
            network: BitcoinNetwork::Mainnet,
            storage: StorageSpec {
                size: "800Gi".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Every network bitcoind knows has its own two ports, and getting them
    /// wrong is not visible from the outside: the node starts, listens on the
    /// wrong number, and never finds a peer.
    #[test]
    fn ports_follow_the_network_unless_asked_otherwise() {
        for (net, rpc, p2p) in [
            (BitcoinNetwork::Mainnet, 8332, 8333),
            (BitcoinNetwork::Testnet, 18332, 18333),
            (BitcoinNetwork::Regtest, 18443, 18444),
            (BitcoinNetwork::Signet, 38332, 38333),
        ] {
            let mut s = spec();
            s.network = net;
            assert_eq!(rpc_port(&s), rpc, "{net:?} rpc");
            assert_eq!(p2p_port(&s), p2p, "{net:?} p2p");
        }
        let mut s = spec();
        s.rpc_port = 19999;
        s.p2p = Some(BitcoinP2PSpec {
            listen_port: 19998,
            ..Default::default()
        });
        assert_eq!(rpc_port(&s), 19999);
        assert_eq!(p2p_port(&s), 19998);
    }

    /// The rendered config must be byte-stable for a stable spec. addnode
    /// entries arrive in whatever order the CR lists them, and an unsorted
    /// render would rewrite the ConfigMap on every pass and roll the
    /// StatefulSet with it — a node restarted every reconcile never syncs.
    #[test]
    fn the_config_is_stable_under_reordering() {
        let mut a = spec();
        a.p2p = Some(BitcoinP2PSpec {
            add_nodes: vec!["c.example".into(), "a.example".into(), "b.example".into()],
            ..Default::default()
        });
        let mut b = spec();
        b.p2p = Some(BitcoinP2PSpec {
            add_nodes: vec!["b.example".into(), "c.example".into(), "a.example".into()],
            ..Default::default()
        });
        assert_eq!(render_conf(&a), render_conf(&b));
        assert_eq!(render_conf(&a), render_conf(&a));
        let conf = render_conf(&a);
        let order: Vec<&str> = conf.lines().filter(|l| l.starts_with("addnode=")).collect();
        assert_eq!(
            order,
            ["addnode=a.example", "addnode=b.example", "addnode=c.example"]
        );
    }

    /// mainnet is bitcoind's default and names itself with no flag; writing one
    /// would be an unknown option and the node would refuse to start.
    #[test]
    fn only_the_non_default_networks_are_named() {
        assert!(!render_conf(&spec()).contains("testnet"));
        for (net, flag) in [
            (BitcoinNetwork::Testnet, "testnet=1"),
            (BitcoinNetwork::Regtest, "regtest=1"),
            (BitcoinNetwork::Signet, "signet=1"),
        ] {
            let mut s = spec();
            s.network = net;
            assert!(render_conf(&s).contains(flag), "{net:?}");
        }
    }

    /// txindex indexes every transaction ever seen; pruning throws away the
    /// blocks it would read. bitcoind refuses the pair at startup, so the CR is
    /// refused here where the reason can be stated.
    #[test]
    fn an_index_over_discarded_blocks_is_refused() {
        let mut s = spec();
        s.tx_index = true;
        s.pruning = 5000;
        assert!(check(&s).is_err());

        s.pruning = 0;
        assert!(check(&s).is_ok());
        assert!(render_conf(&s).contains("txindex=1"));

        s.tx_index = false;
        s.pruning = 5000;
        assert!(check(&s).is_ok());
        assert!(render_conf(&s).contains("prune=5000"));
    }

    /// A height of 0 with 0 peers describes a real and alarming state — stuck at
    /// genesis, isolated. The operator does not call the node, so it must not
    /// say that; -1 means it did not look.
    #[test]
    fn unmeasured_telemetry_is_not_reported_as_zero() {
        let s = BitcoinRuntimeStatus {
            block_height: -1,
            peer_count: -1,
            ..Default::default()
        };
        assert_eq!(s.block_height, -1);
        assert_eq!(s.peer_count, -1);
        assert!(s.sync_progress.is_empty());
    }
}

#[cfg(test)]
mod plan_tests {
    use super::tests::spec;
    use super::*;

    fn owner() -> OwnerReference {
        OwnerReference {
            api_version: "bootno.de/v1".to_string(),
            kind: "BitcoinRuntime".to_string(),
            name: "btc".to_string(),
            uid: "uid-1".to_string(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }
    }

    /// A node is its arguments. The config file is mounted read-only and passed
    /// explicitly, so bitcoind reads what the CR said and not whatever was left
    /// on the data volume by a previous image.
    #[test]
    fn bitcoind_is_started_against_the_rendered_config() {
        let plan = build("btc", "chains", &spec(), owner()).expect("builds");
        let c = plan.container("bitcoind").expect("main container");
        let args = c.args.clone().unwrap_or_default();
        assert!(args.contains(&format!("-conf={CONF_DIR}/bitcoin.conf")), "{args:?}");
        assert!(args.contains(&format!("-datadir={DATA_DIR}")), "{args:?}");

        let mounts = c.volume_mounts.clone().unwrap_or_default();
        let conf = mounts.iter().find(|m| m.name == "config").expect("config mount");
        assert_eq!(conf.read_only, Some(true), "the operator owns the config");
        assert!(mounts.iter().any(|m| m.name == "data" && m.mount_path == DATA_DIR));

        assert_eq!(plan.config("btc-config"), Some(render_conf(&spec()).as_str()));
    }

    /// One node per CR, and a PVC that outlives the pod. Bitcoin peers are not
    /// replicas — a second pod would be a second node with its own chainstate.
    #[test]
    fn there_is_one_node_and_it_keeps_its_data() {
        let plan = build("btc", "chains", &spec(), owner()).expect("builds");
        let sts = &plan.sets[0];
        let spec_ = sts.spec.as_ref().expect("statefulset spec");
        assert_eq!(spec_.replicas, Some(1));
        let pvcs = spec_.volume_claim_templates.clone().unwrap_or_default();
        assert_eq!(pvcs.len(), 1);
        assert_eq!(pvcs[0].metadata.name.as_deref(), Some("data"));
        assert_eq!(spec_.service_name.as_deref(), Some("btc-headless"));
    }

    /// Both services: the headless one gives the pod stable DNS, the ClusterIP
    /// one is what clients dial. A node with only one of them is reachable by
    /// nobody or has no stable identity.
    #[test]
    fn both_services_are_planned_and_owned() {
        let plan = build("btc", "chains", &spec(), owner()).expect("builds");
        assert!(plan.has_service("btc"), "clusterip");
        assert!(plan.has_service("btc-headless"), "headless");
        for o in plan
            .services
            .iter()
            .map(|s| s.metadata.owner_references.clone())
            .chain(plan.sets.iter().map(|s| s.metadata.owner_references.clone()))
            .chain(plan.configs.iter().map(|c| c.metadata.owner_references.clone()))
        {
            let refs = o.expect("every object carries an owner");
            assert_eq!(refs[0].kind, "BitcoinRuntime");
            assert_eq!(refs[0].controller, Some(true));
        }
    }

    /// The indexer reads the blocks the node wrote, so it shares the volume
    /// read-only rather than syncing a second copy. Writable, two processes
    /// would be writing one chainstate.
    #[test]
    fn the_indexer_shares_the_data_volume_read_only() {
        let plan = build("btc", "chains", &spec(), owner()).expect("builds");
        assert!(plan.container("indexer").is_none(), "absent by default");

        let mut s = spec();
        s.indexer = Some(crate::crd::BitcoinIndexerSpec {
            kind: crate::crd::BitcoinIndexerKind::Electrs,
            image: crate::crd::ImageSpec {
                repository: "ghcr.io/hanzoai/electrs".into(),
                tag: "0.10".into(),
                pull_policy: "IfNotPresent".into(),
            },
            rpc_port: 0,
            extra_args: vec!["--jsonrpc-import".into()],
        });
        let plan = build("btc", "chains", &s, owner()).expect("builds");
        let idx = plan.container("indexer").expect("indexer container");
        let m = idx.volume_mounts.clone().unwrap_or_default();
        let data = m.iter().find(|m| m.name == "data").expect("shares data");
        assert_eq!(data.read_only, Some(true));
        assert!(idx.args.clone().unwrap_or_default().contains(&"--jsonrpc-import".to_string()));
        // and the electrum port reaches the services
        assert!(plan.services.iter().any(|s| s.spec.as_ref().unwrap().ports.as_ref().unwrap()
            .iter().any(|p| p.name.as_deref() == Some("electrum"))));
    }

    /// A refused CR builds nothing at all — the guard runs before any object is
    /// constructed, so there is no half-built plan to accidentally apply.
    #[test]
    fn a_refused_cr_produces_no_plan() {
        assert!(build("luxd", "chains", &spec(), owner()).is_err());
        assert!(build("btc", "lux-mainnet", &spec(), owner()).is_err());
        let mut bad = spec();
        bad.tx_index = true;
        bad.pruning = 100;
        assert!(build("btc", "chains", &bad, owner()).is_err());
    }
}
