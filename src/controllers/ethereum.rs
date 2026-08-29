//! EthereumRuntime reconciler — an execution client and a consensus client,
//! deployed as one unit.
//!
//! Post-merge, neither half is a node. The execution client holds state and
//! executes transactions but no longer decides which block is canonical; the
//! consensus client follows the beacon chain but cannot execute anything. They
//! drive each other over the Engine API, authenticated by a JWT they both read.
//!
//! So this is one CR and two StatefulSets. Splitting it into two CRs would let
//! someone create half a node — an execution client with nothing to tell it
//! what to build on — which is not a degraded node, it is a stopped one.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, Service as CoreService, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, Patch, PatchParams};
use kube::runtime::controller::{Action, Controller};
use kube::runtime::watcher::Config;
use kube::{Client, Resource, ResourceExt};
use tracing::{error, info, warn};

use crate::apply;
use crate::core::{OperatorError, Result};
use crate::crd::{
    EthereumRuntime, EthereumRuntimeSpec, EthereumRuntimeStatus, Phase,
    ServicePort as CrServicePort, StorageSpec,
};
use crate::crd_types::build_condition;
use crate::manifests;

use super::chain::{
    conditions_equivalent, config_volume, local_refs, or_else, secret_volume, tcp_probe,
};
use super::owner_ref_for;

/// Where the shared JWT is mounted, on both layers, under the same name — the
/// two clients have to agree on the file, not just the secret.
const JWT_DIR: &str = "/etc/ethereum/jwt";
const JWT_FILE: &str = "jwt.hex";
const EL_DATA_DIR: &str = "/data/execution";
const CL_DATA_DIR: &str = "/data/consensus";
const CONF_DIR: &str = "/etc/ethereum/config";
/// devp2p and the beacon-chain gossip port. Fixed rather than configurable
/// because peers find each other on them.
const EL_P2P_PORT: i32 = 30303;
const CL_P2P_PORT: i32 = 9000;

#[derive(Clone)]
pub struct Ctx {
    pub client: Client,
    pub api_group: String,
}

/// The two layers' child names. Deterministic, so a reconcile adopts what the
/// last one built instead of creating a second copy beside it.
pub fn el_name(name: &str) -> String {
    format!("{name}-el")
}
pub fn cl_name(name: &str) -> String {
    format!("{name}-cl")
}

/// The Engine API port. 8551 by convention, and both layers must resolve it the
/// same way — it is the port one dials and the other listens on.
pub fn auth_rpc_port(spec: &EthereumRuntimeSpec) -> i32 {
    if spec.auth_rpc_port != 0 {
        spec.auth_rpc_port
    } else {
        8551
    }
}
pub fn el_rpc_port(spec: &EthereumRuntimeSpec) -> i32 {
    if spec.el_rpc_port != 0 {
        spec.el_rpc_port
    } else {
        8545
    }
}
pub fn cl_rpc_port(spec: &EthereumRuntimeSpec) -> i32 {
    if spec.cl_rpc_port != 0 {
        spec.cl_rpc_port
    } else {
        5052
    }
}

/// Where the consensus client dials the execution client.
pub fn engine_url(name: &str, namespace: &str, auth_port: i32) -> String {
    format!("http://{}.{}.svc:{}", el_name(name), namespace, auth_port)
}

/// The resolved wiring, as ConfigMap data. A pure function of the spec, so an
/// unchanged CR renders identical bytes and applies to nothing.
pub fn render_config(
    name: &str,
    namespace: &str,
    spec: &EthereumRuntimeSpec,
    auth_port: i32,
) -> BTreeMap<String, String> {
    let mut env = String::new();
    env.push_str(&format!("ETH_NETWORK={}\n", value_of(&spec.network)));
    env.push_str(&format!("EL_KIND={}\n", value_of(&spec.execution.kind)));
    env.push_str(&format!("CL_KIND={}\n", value_of(&spec.consensus.kind)));
    env.push_str(&format!(
        "EL_SYNC_MODE={}\n",
        or_else(&spec.execution.sync_mode, "snap")
    ));
    env.push_str(&format!(
        "ENGINE_API_URL={}\n",
        engine_url(name, namespace, auth_port)
    ));
    if !spec.fee_recipient.is_empty() {
        env.push_str(&format!("FEE_RECIPIENT={}\n", spec.fee_recipient));
    }
    if !spec.consensus.checkpoint_sync.is_empty() {
        env.push_str(&format!(
            "CHECKPOINT_SYNC_URL={}\n",
            spec.consensus.checkpoint_sync
        ));
    }
    let mut out = BTreeMap::new();
    out.insert("ethereum.env".to_string(), env);
    out
}

/// The wire spelling of an enum value — what the CR said, not the Rust name.
fn value_of<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|j| j.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The pair authenticates over a shared secret. Without it the Engine API
/// refuses every call and the node never builds a block — so the CR is refused
/// here rather than deployed into a silent stall.
fn check(spec: &EthereumRuntimeSpec) -> Result<()> {
    if spec.jwt_secret.name.is_empty() {
        return Err(OperatorError::Config(
            "jwtSecret names the secret the two layers authenticate with; without it the Engine API refuses every call".into(),
        ));
    }
    Ok(())
}

pub async fn reconcile(cr: Arc<EthereumRuntime>, ctx: Arc<Ctx>) -> Result<Action> {
    let name = cr.name_any();
    let namespace = cr
        .namespace()
        .ok_or_else(|| OperatorError::Config("EthereumRuntime has no namespace".into()))?;
    let api_version = format!("{}/v1", ctx.api_group);
    let owner = owner_ref_for(cr.as_ref(), &api_version, "EthereumRuntime");
    reconcile_inner(&ctx.client, &name, &namespace, &cr.spec, owner).await?;
    write_status(&ctx.client, &name, &namespace, &cr).await;
    Ok(Action::requeue(Duration::from_secs(60)))
}

async fn reconcile_inner(
    client: &Client,
    name: &str,
    namespace: &str,
    spec: &EthereumRuntimeSpec,
    owner: OwnerReference,
) -> Result<()> {
    check(spec)?;

    let base = manifests::standard_labels(name, "ethereum", "ethereum", &spec.execution.image.tag);
    let auth = auth_rpc_port(spec);
    let conf_name = format!("{name}-config");

    let mut cm = manifests::build_configmap(
        &conf_name,
        namespace,
        base.clone(),
        render_config(name, namespace, spec, auth),
    );
    cm.metadata.owner_references = Some(vec![owner.clone()]);
    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
    apply::apply_configmap(&cms, &cm).await?;

    // --- execution ---
    let el = el_name(name);
    let el_ports = vec![
        port("eljson", el_rpc_port(spec)),
        port("engine", auth),
        port("p2p", EL_P2P_PORT),
    ];
    let mut el_args = vec![
        format!("--datadir={EL_DATA_DIR}"),
        format!("--authrpc.jwtsecret={JWT_DIR}/{JWT_FILE}"),
        format!("--authrpc.port={auth}"),
    ];
    el_args.extend(spec.execution.extra_args.iter().cloned());
    let el_container = manifests::build_container(
        "execution",
        &manifests::image_ref(
            &spec.execution.image.repository,
            &spec.execution.image.tag,
        ),
        &spec.execution.image.pull_policy,
        vec![],
        el_args,
        vec![],
        vec![],
        layer_mounts(EL_DATA_DIR),
        manifests::container_ports(&el_ports),
        spec.resources.as_ref().map(manifests::to_k8s_resources),
        None,
        Some(tcp_probe(auth, 30, 15)),
    );
    apply_layer(
        client,
        &el,
        namespace,
        layer_labels(&base, &el, "execution"),
        el_container,
        vec![
            secret_volume("jwt", &spec.jwt_secret.name),
            config_volume("config", &conf_name),
        ],
        &spec.execution.storage,
        &spec.image_pull_secrets,
        &el_ports,
        &owner,
    )
    .await?;

    // --- consensus ---
    let cl = cl_name(name);
    let cl_ports = vec![port("beacon", cl_rpc_port(spec)), port("p2p", CL_P2P_PORT)];
    let mut cl_args = vec![
        format!("--datadir={CL_DATA_DIR}"),
        format!("--execution-endpoint={}", engine_url(name, namespace, auth)),
        format!("--execution-jwt={JWT_DIR}/{JWT_FILE}"),
    ];
    cl_args.extend(spec.consensus.extra_args.iter().cloned());
    if !spec.consensus.checkpoint_sync.is_empty() {
        cl_args.push(format!(
            "--checkpoint-sync-url={}",
            spec.consensus.checkpoint_sync
        ));
    }
    if !spec.fee_recipient.is_empty() {
        cl_args.push(format!("--suggested-fee-recipient={}", spec.fee_recipient));
    }
    let cl_container = manifests::build_container(
        "consensus",
        &manifests::image_ref(
            &spec.consensus.image.repository,
            &spec.consensus.image.tag,
        ),
        &spec.consensus.image.pull_policy,
        vec![],
        cl_args,
        vec![],
        vec![],
        layer_mounts(CL_DATA_DIR),
        manifests::container_ports(&cl_ports),
        spec.resources.as_ref().map(manifests::to_k8s_resources),
        None,
        Some(tcp_probe(cl_rpc_port(spec), 30, 15)),
    );
    apply_layer(
        client,
        &cl,
        namespace,
        layer_labels(&base, &cl, "consensus"),
        cl_container,
        vec![
            secret_volume("jwt", &spec.jwt_secret.name),
            config_volume("config", &conf_name),
        ],
        &spec.consensus.storage,
        &spec.image_pull_secrets,
        &cl_ports,
        &owner,
    )
    .await?;

    info!(name, namespace, network = ?spec.network, "EthereumRuntime reconciled");
    Ok(())
}

fn port(name: &str, container_port: i32) -> CrServicePort {
    CrServicePort {
        name: name.to_string(),
        container_port,
        service_port: None,
        protocol: "TCP".to_string(),
    }
}

fn layer_mounts(data_dir: &str) -> Vec<VolumeMount> {
    vec![
        VolumeMount {
            name: "data".to_string(),
            mount_path: data_dir.to_string(),
            ..Default::default()
        },
        VolumeMount {
            name: "jwt".to_string(),
            mount_path: JWT_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
        VolumeMount {
            name: "config".to_string(),
            mount_path: CONF_DIR.to_string(),
            read_only: Some(true),
            ..Default::default()
        },
    ]
}

fn layer_labels(
    base: &BTreeMap<String, String>,
    layer: &str,
    component: &str,
) -> BTreeMap<String, String> {
    let mut own = BTreeMap::new();
    own.insert(manifests::LABEL_NAME.to_string(), layer.to_string());
    own.insert(manifests::LABEL_INSTANCE.to_string(), layer.to_string());
    own.insert(manifests::LABEL_COMPONENT.to_string(), component.to_string());
    manifests::merge_labels(&[base, &own])
}

/// One layer: a single-replica StatefulSet with its own PVC, plus the headless
/// and ClusterIP Services. The two layers differ only in what is passed here.
#[allow(clippy::too_many_arguments)]
async fn apply_layer(
    client: &Client,
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    container: Container,
    volumes: Vec<Volume>,
    storage: &StorageSpec,
    pull: &[String],
    ports: &[CrServicePort],
    owner: &OwnerReference,
) -> Result<()> {
    let sel = manifests::selector_labels(name);
    let pvc = manifests::build_pvc_template("data", &storage.storage_class_name, &storage.size);
    let headless = format!("{name}-headless");
    let mut sts = manifests::build_statefulset(
        name,
        namespace,
        labels.clone(),
        sel.clone(),
        Some(1),
        vec![container],
        volumes,
        vec![pvc],
        local_refs(pull),
        &headless,
    );
    sts.metadata.owner_references = Some(vec![owner.clone()]);
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    apply::apply(&stss, &sts).await?;

    let svc_ports = manifests::service_ports(ports);
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
    clip.metadata.owner_references = Some(vec![owner.clone()]);
    apply::apply_service(&svcs, &clip).await?;
    Ok(())
}

async fn ready(client: &Client, name: &str, namespace: &str) -> bool {
    let stss: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
    stss.get_opt(name)
        .await
        .ok()
        .flatten()
        .and_then(|sts| sts.status)
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0)
        >= 1
}

/// The two layers report separately on purpose. An execution client can be
/// fully synced while the beacon node is still backfilling, and in that state
/// the node serves nothing — one aggregate boolean would call it healthy.
async fn compute_status(
    client: &Client,
    name: &str,
    namespace: &str,
    cr: &EthereumRuntime,
) -> EthereumRuntimeStatus {
    let el_ready = ready(client, &el_name(name), namespace).await;
    let cl_ready = ready(client, &cl_name(name), namespace).await;
    let both = el_ready && cl_ready;
    let generation = cr.meta().generation.unwrap_or(0);
    let mut status = EthereumRuntimeStatus {
        phase: Some(if both { Phase::Running } else { Phase::Creating }),
        el_ready,
        cl_ready,
        slot_height: -1,
        peer_count: -1,
        observed_generation: generation,
        ..Default::default()
    };
    status.conditions.push(build_condition(
        "Ready",
        both,
        if both { "Available" } else { "NotReady" },
        &format!("execution ready={el_ready}, consensus ready={cl_ready}"),
        generation,
    ));
    status
}

async fn write_status(client: &Client, name: &str, namespace: &str, cr: &EthereumRuntime) {
    let next = compute_status(client, name, namespace, cr).await;
    if let Some(current) = &cr.status {
        if current.phase == next.phase
            && current.el_ready == next.el_ready
            && current.cl_ready == next.cl_ready
            && current.slot_height == next.slot_height
            && current.peer_count == next.peer_count
            && current.sync_progress == next.sync_progress
            && current.observed_generation == next.observed_generation
            && conditions_equivalent(&current.conditions, &next.conditions)
        {
            return;
        }
    }
    let api: Api<EthereumRuntime> = Api::namespaced(client.clone(), namespace);
    let patch = serde_json::json!({ "status": next });
    let pp = PatchParams::apply(apply::FIELD_MANAGER);
    if let Err(e) = api.patch_status(name, &pp, &Patch::Merge(&patch)).await {
        warn!(error = %e, "failed to update EthereumRuntime status");
    }
}

pub fn on_error(_obj: Arc<EthereumRuntime>, err: &OperatorError, _ctx: Arc<Ctx>) -> Action {
    error!(error = %err, "EthereumRuntime reconcile failed");
    Action::requeue(Duration::from_secs(30))
}

pub async fn run_ethereum_controller(client: Client, namespace: String, api_group: String) {
    let api: Api<EthereumRuntime> = if namespace.is_empty() {
        Api::all(client.clone())
    } else {
        Api::namespaced(client.clone(), &namespace)
    };
    info!(group = %api_group, "Starting EthereumRuntime controller");
    let ctx = Arc::new(Ctx { client, api_group });
    Controller::new(api, Config::default())
        .run(reconcile, on_error, ctx)
        .for_each(|res| async move {
            if let Err(e) = res {
                warn!(error = %e, "EthereumRuntime reconcile error");
            }
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        ConsensusKind, EthereumConsensusSpec, EthereumExecutionSpec, EthereumNetwork,
        ExecutionKind, ImageSpec, SecretRef,
    };

    fn spec() -> EthereumRuntimeSpec {
        EthereumRuntimeSpec {
            network: EthereumNetwork::Mainnet,
            execution: EthereumExecutionSpec {
                kind: ExecutionKind::Geth,
                image: ImageSpec {
                    repository: "ghcr.io/hanzoai/geth".to_string(),
                    tag: "v1.15.0".to_string(),
                    pull_policy: "IfNotPresent".to_string(),
                },
                storage: StorageSpec {
                    size: "2Ti".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            consensus: EthereumConsensusSpec {
                kind: ConsensusKind::Lighthouse,
                image: ImageSpec {
                    repository: "ghcr.io/hanzoai/lighthouse".to_string(),
                    tag: "v6.0.0".to_string(),
                    pull_policy: "IfNotPresent".to_string(),
                },
                storage: StorageSpec {
                    size: "300Gi".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            jwt_secret: SecretRef {
                name: "eth-jwt".to_string(),
                key: String::new(),
            },
            ..Default::default()
        }
    }

    /// The consensus client dials the execution client's Engine API. If the two
    /// resolve that port differently, one listens where the other never calls
    /// and the pair never talks — with both pods reporting healthy.
    #[test]
    fn both_layers_resolve_the_engine_port_the_same_way() {
        let s = spec();
        assert_eq!(auth_rpc_port(&s), 8551);
        assert!(engine_url("eth", "chains", auth_rpc_port(&s)).ends_with(":8551"));

        let mut custom = spec();
        custom.auth_rpc_port = 18551;
        assert_eq!(auth_rpc_port(&custom), 18551);
        assert_eq!(
            engine_url("eth", "chains", auth_rpc_port(&custom)),
            "http://eth-el.chains.svc:18551"
        );
        // The URL names the execution layer's Service, not the CR.
        assert!(engine_url("eth", "chains", 8551).contains(&el_name("eth")));
    }

    /// Defaults, spelled once. 8545 and 5052 are what tooling assumes.
    #[test]
    fn the_rpc_ports_default_to_the_conventional_ones() {
        let s = spec();
        assert_eq!(el_rpc_port(&s), 8545);
        assert_eq!(cl_rpc_port(&s), 5052);
        let mut c = spec();
        c.el_rpc_port = 9545;
        c.cl_rpc_port = 6052;
        assert_eq!(el_rpc_port(&c), 9545);
        assert_eq!(cl_rpc_port(&c), 6052);
    }

    /// The rendered wiring carries what the CR chose, spelled as the wire
    /// spells it — `geth`, not `Geth` — and resolves the sync mode default.
    #[test]
    fn the_config_carries_wire_spellings() {
        let conf = render_config("eth", "chains", &spec(), 8551);
        let env = &conf["ethereum.env"];
        assert!(env.contains("ETH_NETWORK=mainnet"), "{env}");
        assert!(env.contains("EL_KIND=geth"), "{env}");
        assert!(env.contains("CL_KIND=lighthouse"), "{env}");
        assert!(env.contains("EL_SYNC_MODE=snap"), "{env}");
        assert!(env.contains("ENGINE_API_URL=http://eth-el.chains.svc:8551"), "{env}");
        // Absent optional fields say nothing rather than saying empty.
        assert!(!env.contains("FEE_RECIPIENT="), "{env}");
        assert!(!env.contains("CHECKPOINT_SYNC_URL="), "{env}");
        // Stable for a stable spec.
        assert_eq!(conf, render_config("eth", "chains", &spec(), 8551));
    }

    /// Without the shared secret the Engine API rejects every call, so the pair
    /// deploys, reports healthy, and produces nothing. Refuse it where the
    /// reason can be given.
    #[test]
    fn a_pair_with_no_shared_secret_is_refused() {
        let mut s = spec();
        s.jwt_secret.name = String::new();
        assert!(check(&s).is_err());
        assert!(check(&spec()).is_ok());
    }

    /// Child names are derived, so a second reconcile adopts what the first
    /// built rather than creating another pair beside it.
    #[test]
    fn layer_names_are_derived_from_the_cr() {
        assert_eq!(el_name("mainnet"), "mainnet-el");
        assert_eq!(cl_name("mainnet"), "mainnet-cl");
        assert_ne!(el_name("mainnet"), cl_name("mainnet"));
    }
}
