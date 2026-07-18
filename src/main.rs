//! Hanzo Operator — Rust port (canonical for all universes).
//!
//! Manages 28 CRD Kinds at a configurable API group (default `hanzo.ai`).
//! One binary serves Hanzo, Lux, Zoo, and Osage universes (and any
//! white-label tenant) via
//! `--api-group` / `OPERATOR_API_GROUP`.
//!
//! See `~/work/hanzo/operator/README.md` for install + CRD reference.

#![allow(dead_code)]

mod api_group;
mod apply;
mod controllers;
mod core;
mod crd;
mod crd_types;
mod gitops;
mod install;
mod manifests;
mod registry;
mod zapclient;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{
    routing::get,
    Router,
};
use clap::Parser;
use kube::Client;
use std::net::SocketAddr;
use tracing::{info, warn};

use crate::api_group::ApiGroup;
use crate::core::{LeaderConfig, LeaderElection};

#[derive(Parser, Debug, Clone)]
#[command(name = "operator")]
#[command(about = "Kubernetes operator for Hanzo platform (canonical, all universes)", long_about = None)]
struct Args {
    /// Log level: trace, debug, info, warn, error.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Namespace to watch. Empty = all namespaces (requires ClusterRole).
    #[arg(long, env = "WATCH_NAMESPACE", default_value = "")]
    namespace: String,

    /// API group for CRDs. Overrides compile-time default `hanzo.ai`.
    /// Other universes: `lux.cloud`, `zoo.cloud`, `osage.cloud`.
    #[arg(long, env = "OPERATOR_API_GROUP", global = true)]
    api_group: Option<String>,

    /// Health-check listener address.
    #[arg(long, env = "HEALTH_ADDR", default_value = "0.0.0.0:8081")]
    health_addr: String,

    /// Enable lease-based leader election. Set false on local dev for
    /// single-replica runs.
    #[arg(long, env = "LEADER_ELECT", default_value = "true")]
    leader_election: bool,

    /// Operator namespace (where the Lease object lives).
    #[arg(
        long,
        env = "OPERATOR_NAMESPACE",
        default_value = "hanzo-operator-system",
        global = true
    )]
    operator_namespace: String,

    /// Optional subcommand. Absent ⇒ run the reconcile loop (the operator).
    #[command(subcommand)]
    command: Option<Command>,
}

/// The install verb — bootstrap the operator into a cluster. Absent ⇒ the
/// binary runs the reconcile loop.
#[derive(clap::Subcommand, Debug, Clone)]
enum Command {
    /// Install the derived CRDs + the operator's own RBAC/Deployment into the
    /// current-context cluster (`hanzod install` execs into this).
    Install(InstallOpts),
    /// Bootstrap: install the CRDs + operator, then apply the platform's own App
    /// CRs so the running operator brings the whole stack up (`hanzo up`).
    Up(UpOpts),
}

#[derive(clap::Args, Debug, Clone)]
struct InstallOpts {
    /// Operator image for the rendered Deployment. Default: this binary's own
    /// pinned version (`ghcr.io/hanzoai/operator:v<version>`).
    #[arg(long, env = "OPERATOR_IMAGE")]
    image: Option<String>,
    /// Enable the managed-upgrade FSM on the installed operator
    /// (`UPGRADE_FSM_ENABLED=true`). Default off — a safe drop-in.
    #[arg(long)]
    upgrade_fsm: bool,
    /// Install ONLY the CRDs (skip the operator Deployment/RBAC).
    #[arg(long)]
    crds_only: bool,
}

#[derive(clap::Args, Debug, Clone)]
struct UpOpts {
    /// Operator image for the rendered Deployment. Default: this binary's own
    /// pinned version.
    #[arg(long, env = "OPERATOR_IMAGE")]
    image: Option<String>,
    /// Enable the managed-upgrade FSM on the installed operator. Default off.
    #[arg(long)]
    upgrade_fsm: bool,
    /// Directory of platform App-CR YAML to apply (the stack the operator brings
    /// up). Absent ⇒ install the operator only.
    #[arg(long)]
    manifests: Option<std::path::PathBuf>,
}

/// Shared state for the health server: leadership (for `/readyz`).
#[derive(Clone)]
struct HealthState {
    leader: Arc<AtomicBool>,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<HealthState>,
) -> (axum::http::StatusCode, &'static str) {
    if state.leader.load(Ordering::Relaxed) {
        (axum::http::StatusCode::OK, "ready")
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not leader")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls 0.23 compiles both aws-lc-rs (reqwest) and ring (kube) providers, so
    // the process-level CryptoProvider is ambiguous and the first TLS handshake
    // panics. Install aws-lc-rs explicitly before any client is built.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("install rustls aws-lc-rs CryptoProvider");

    let args = Args::parse();

    // Logging.
    let filter = tracing_subscriber::EnvFilter::try_new(&args.log_level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let api_group = ApiGroup::resolve(args.api_group.as_deref());
    info!(
        version = env!("CARGO_PKG_VERSION"),
        api_group = %api_group.group,
        namespace = %args.namespace,
        leader_election = args.leader_election,
        "Starting Hanzo Operator"
    );

    let client = Client::try_default().await?;
    info!("Connected to Kubernetes cluster");

    // Install verbs — bootstrap the operator into the cluster, then exit. Absent
    // ⇒ fall through to the reconcile loop (the running operator).
    if let Some(command) = args.command.clone() {
        return run_command(command, &client, &api_group.group, &args.operator_namespace).await;
    }

    // Shutdown channel.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Leader election.
    let leader_election = LeaderElection::new(
        client.clone(),
        args.operator_namespace.clone(),
        LeaderConfig {
            lease_name: "hanzo-operator-leader".to_string(),
            identity_prefix: "hanzo-operator-".to_string(),
        },
    );
    let leader_flag = leader_election.leader_flag();
    if !args.leader_election {
        leader_flag.store(true, Ordering::Relaxed);
        info!("Leader election disabled, running as leader");
    }

    // Health server.
    let health_addr: SocketAddr = args.health_addr.parse()?;
    let health_app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(HealthState {
            leader: leader_flag.clone(),
        });

    let group = api_group.group.clone();
    let namespace = args.namespace.clone();
    let controllers_flag = leader_flag.clone();

    tokio::select! {
        // Leader election loop (only if enabled).
        _ = async {
            if args.leader_election {
                leader_election.run(shutdown_rx.clone()).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            info!("Leader election exited");
        }

        // Controllers — wait for leadership then run all of them.
        _ = run_all_controllers(client.clone(), namespace.clone(), group.clone(), controllers_flag.clone()) => {
            warn!("Controllers exited");
        }

        // Health server.
        res = axum::serve(
            tokio::net::TcpListener::bind(health_addr).await?,
            health_app.into_make_service(),
        ) => {
            if let Err(e) = res {
                tracing::error!(error = %e, "Health server exited");
            }
        }

        // Graceful shutdown on SIGINT (Ctrl-C) or SIGTERM (what k8s sends on pod
        // stop). Handling SIGTERM lets the leader release its lease promptly on a
        // rolling operator restart instead of the successor waiting out the full
        // lease timeout (MED-2).
        _ = shutdown_signal() => {
            info!("Received shutdown signal");
        }
    }

    let _ = shutdown_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    info!("Operator stopped");
    Ok(())
}

/// Resolve when the process receives a termination signal: SIGINT (Ctrl-C) or
/// SIGTERM (Kubernetes pod stop). Both drive the graceful shutdown that releases
/// the leader lease.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT handler");
        tokio::select! {
            _ = sigterm.recv() => {}
            _ = sigint.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Wait for leadership then run every controller in parallel.
async fn run_all_controllers(
    client: Client,
    namespace: String,
    api_group: String,
    leader_flag: Arc<AtomicBool>,
) {
    // Block until we become the leader.
    loop {
        if leader_flag.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    info!("Acquired leadership, starting all controllers");

    // Spawn every controller. `tokio::select!` won't help here because we
    // want them ALL to run concurrently for the operator's lifetime.
    tokio::join!(
        // Canonical Kinds.
        controllers::service::run_service_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone(),
            leader_flag.clone()
        ),
        // Pre-flight orphan GC (MED-1) — startup + periodic reclaimer of leaked
        // managed-upgrade pre-flight resources (clone PVC / VolumeSnapshot are
        // full copies of live tenant data). Leader-gated; a cheap no-op when the
        // FSM never ran.
        controllers::service::run_preflight_gc(
            client.clone(),
            namespace.clone(),
            leader_flag.clone()
        ),
        controllers::datastore::run_datastore_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::gateway::run_gateway_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::mpc::run_mpc_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::network::run_network_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::ingress::run_ingress_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::dns::run_dns_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::base::run_base_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::queue::run_queue_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::observability::run_observability_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::function::run_function_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::spa::run_spa_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::static_site::run_static_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        // Image automation (retires notify-universe dispatch): watches registries
        // and writes image-tag bumps back to git. The git→cluster delivery half is
        // now the cloud deploy engine (/v1/deploy), not a second reconciler here.
        controllers::imageupdate::run_imageupdate_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        // v0.3.3: facade Kinds.
        controllers::sql::run_sql_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::kv::run_kv_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::docdb::run_docdb_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::s3::run_s3_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::iam::run_iam_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::kms::run_kms_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::llm::run_llm_controller(client.clone(), namespace.clone(), api_group.clone()),
        controllers::indexer::run_indexer_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::explorer::run_explorer_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        // v0.3.4: union with go/ — blockchain Kinds.
        controllers::luxruntime::run_luxruntime_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::nodefleet::run_nodefleet_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        // ManagedDatabase facade — per-tenant isolated Datastore workload.
        controllers::managed_database::run_managed_database_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        // App Kind — the role-dispatch super-facade. The App-collapse: the fleet's
        // workload CRs are `kind: App`; this controller dispatches on `spec.role`
        // to the existing per-profile reconciles (service/datastore/dns/ingress)
        // threaded with the App's owner reference, so SSA-by-name adopts the
        // existing Deployment/StatefulSet (Service→App) with no recreate. It is the
        // sole hanzo.ai workload reconciler for the collapsed fleet.
        controllers::app::run_app_controller(client.clone(), namespace.clone(), api_group.clone()),
        // AgentDeployment — autonomous-bot lifecycle. Watches the CRD; its
        // reconcile ACTIONS reach cloud /v1/agents + visor /v1/machines over
        // HTTP. Provisioning is opt-in + fail-safe: without AGENT_DEPLOY_CLOUD_URL
        // / AGENT_DEPLOY_VISOR_URL / token it runs READ-ONLY, and even configured
        // it never launches a machine unless AGENT_DEPLOY_MODE=on.
        controllers::agent_deployment::run_agent_deployment_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        // Additive ZAP-native KMS secret projector — opt-in (off unless
        // KMS_ZAP_CONTROLLER=true). Watches the fixed kms.hanzo.ai KMSSecret
        // family; ignores non-zap-native CRs so the REST projector is untouched.
        controllers::kms_zap::run_kms_zap_controller(
            client.clone(),
            namespace.clone(),
            std::env::var("KMS_ZAP_CONTROLLER")
                .map(|v| v == "true")
                .unwrap_or(false),
        ),
        // Tenant controller — projects the namespace-scoped
        // `cloud-api-platform` RoleBinding + `ghcr-pull` image-pull Secret into
        // every platform-managed tenant namespace so an onboarded org gets
        // one-click `/v1/platform` deploy scoped to ITS OWN namespace (never a
        // ClusterRoleBinding). Cloud's deploy path BLOCKS on it (waitForTenantRBAC).
        // Enabled by default; set TENANT_CONTROLLER=false to disable.
        controllers::tenant::run_tenant_controller(
            client.clone(),
            std::env::var("TENANT_CONTROLLER")
                .map(|v| v != "false")
                .unwrap_or(true),
        ),
    );
}

/// Execute an install verb, then exit. Every step is a server-side apply, so
/// `install` / `up` are idempotent and safe to re-run.
async fn run_command(
    command: Command,
    client: &Client,
    group: &str,
    operator_namespace: &str,
) -> anyhow::Result<()> {
    match command {
        Command::Install(o) => {
            let n = install::install_crds(client, group).await?;
            info!(count = n, group, "installed CRDs");
            if o.crds_only {
                info!("--crds-only: skipped the operator Deployment/RBAC");
            } else {
                let image = o.image.unwrap_or_else(default_operator_image);
                install::install_operator(client, operator_namespace, &image, group, o.upgrade_fsm)
                    .await?;
                info!(
                    namespace = operator_namespace,
                    image = %image,
                    upgrade_fsm = o.upgrade_fsm,
                    "installed operator (RBAC + Deployment)"
                );
            }
        }
        Command::Up(o) => {
            let n = install::install_crds(client, group).await?;
            info!(count = n, group, "installed CRDs");
            let image = o.image.unwrap_or_else(default_operator_image);
            install::install_operator(client, operator_namespace, &image, group, o.upgrade_fsm)
                .await?;
            info!(namespace = operator_namespace, image = %image, "installed operator");
            match &o.manifests {
                Some(dir) => {
                    let applied = install::apply_manifest_dir(client, dir).await?;
                    info!(count = applied, dir = %dir.display(), "applied platform App CRs; the operator will bring them up");
                }
                None => info!(
                    "operator is up — apply the platform App CRs (--manifests <dir>) to bring the stack up"
                ),
            }
        }
    }
    Ok(())
}

/// Default operator image: this binary's own pinned version (never `:latest`).
fn default_operator_image() -> String {
    format!(
        "{}:v{}",
        install::DEFAULT_OPERATOR_IMAGE,
        env!("CARGO_PKG_VERSION")
    )
}
