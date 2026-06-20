//! Hanzo Operator — Rust port (canonical for all universes).
//!
//! Manages 29 CRD Kinds at a configurable API group (default `hanzo.ai`).
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
mod manifests;
mod zapclient;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use axum::{routing::get, Router};
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
    #[arg(long, env = "OPERATOR_API_GROUP")]
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
        default_value = "hanzo-operator-system"
    )]
    operator_namespace: String,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(
    axum::extract::State(state): axum::extract::State<Arc<AtomicBool>>,
) -> (axum::http::StatusCode, &'static str) {
    if state.load(Ordering::Relaxed) {
        (axum::http::StatusCode::OK, "ready")
    } else {
        (axum::http::StatusCode::SERVICE_UNAVAILABLE, "not leader")
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
        .with_state(leader_flag.clone());

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

        // Graceful shutdown on Ctrl-C.
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
        }
    }

    let _ = shutdown_tx.send(true);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    info!("Operator stopped");
    Ok(())
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
            api_group.clone()
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
        // v0.3.4: union with go/ — Hanzo backcompat aliases + blockchain Kinds.
        controllers::hanzo_service::run_hanzo_service_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::hanzo_datastore::run_hanzo_datastore_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::hanzo_dns::run_hanzo_dns_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::luxnetwork::run_luxnetwork_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
        controllers::nodefleet::run_nodefleet_controller(
            client.clone(),
            namespace.clone(),
            api_group.clone()
        ),
    );
}
