//! Shared helpers for the foreign-chain runtimes — bitcoind, Ethereum, Solana.
//!
//! One home rather than three copies. These are the pieces a chain node needs
//! and the cloud workloads do not: a credential projected out of a Secret, a
//! keypair mounted as a directory, a readiness check that is a open TCP port
//! rather than an HTTP route, because a syncing node answers a socket long
//! before it answers a request.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{
    ConfigMap, Container, EnvVarSource, LocalObjectReference, Probe, SecretKeySelector,
    SecretVolumeSource, Service as CoreService, TCPSocketAction, Volume,
};
use kube::api::Api;
use kube::Client;

use crate::core::{OperatorError, Result};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::crd_types::Condition;

/// Project one key of a Secret into an env var.
pub fn secret_key_ref(secret: &str, key: &str) -> EnvVarSource {
    EnvVarSource {
        secret_key_ref: Some(SecretKeySelector {
            name: secret.to_string(),
            key: key.to_string(),
            optional: None,
        }),
        ..Default::default()
    }
}

/// Mount a whole Secret as a directory — how a node takes a keypair or a JWT.
pub fn secret_volume(volume: &str, secret: &str) -> Volume {
    Volume {
        name: volume.to_string(),
        secret: Some(SecretVolumeSource {
            secret_name: Some(secret.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The first non-empty of two, for a spec field that defaults per call site.
pub fn or_else<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.is_empty() {
        fallback
    } else {
        value
    }
}

pub fn local_refs(names: &[String]) -> Vec<LocalObjectReference> {
    names
        .iter()
        .map(|n| LocalObjectReference { name: n.clone() })
        .collect()
}

/// A readiness probe on an open port.
///
/// Chain nodes get generous delays because catching up is not a fault. A
/// bitcoind replaying blocks or a beacon node backfilling is working exactly as
/// intended, and a probe tuned for a web service would restart it repeatedly
/// and guarantee it never finishes.
pub fn tcp_probe(port: i32, initial_delay: i32, period: i32) -> Probe {
    Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(initial_delay),
        period_seconds: Some(period),
        timeout_seconds: Some(5),
        failure_threshold: Some(6),
        success_threshold: Some(1),
        ..Default::default()
    }
}

/// Whether two condition lists say the same thing.
///
/// Compared by (type, status, reason) and not by timestamp or message: a
/// condition re-observed unchanged would otherwise dirty the status on every
/// pass, and each write wakes the watch that scheduled the pass.
pub fn conditions_equivalent(a: &[Condition], b: &[Condition]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.type_ == y.type_ && x.status == y.status && x.reason == y.reason
        })
}

/// A ConfigMap volume by name.
pub fn config_volume(volume: &str, config_map: &str) -> Volume {
    Volume {
        name: volume.to_string(),
        config_map: Some(k8s_openapi::api::core::v1::ConfigMapVolumeSource {
            name: config_map.to_string(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// `data` map for a single-file ConfigMap.
pub fn one_file(name: &str, body: String) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(name.to_string(), body);
    m
}

// ---------------------------------------------------------------------------
// What a CR resolves to, before anything is sent
// ---------------------------------------------------------------------------

/// The objects one chain CR becomes.
///
/// Building and applying were one function, which meant the only way to ask
/// what a CR produces was to have a cluster take it. Deciding is pure and
/// sending is not, so they are separate: a test can now assert the arguments a
/// node will actually be started with, and that is the part that is wrong when
/// a node comes up misconfigured.
#[derive(Default, Debug)]
pub struct Plan {
    pub configs: Vec<ConfigMap>,
    pub sets: Vec<StatefulSet>,
    pub services: Vec<CoreService>,
}

impl Plan {
    /// Send it. The order matters: config before the workload that mounts it,
    /// so a pod is never scheduled against a ConfigMap that is not there yet.
    pub async fn apply(&self, client: &Client, namespace: &str) -> Result<()> {
        let cms: Api<ConfigMap> = Api::namespaced(client.clone(), namespace);
        for cm in &self.configs {
            crate::apply::apply_configmap(&cms, cm).await?;
        }
        let sets: Api<StatefulSet> = Api::namespaced(client.clone(), namespace);
        for sts in &self.sets {
            crate::apply::apply(&sets, sts).await?;
        }
        let svcs: Api<CoreService> = Api::namespaced(client.clone(), namespace);
        for svc in &self.services {
            crate::apply::apply_service(&svcs, svc).await?;
        }
        Ok(())
    }

    /// The container by name, across every set in the plan. Tests reach for
    /// this constantly; a chain node IS its container arguments.
    pub fn container(&self, name: &str) -> Option<&Container> {
        self.sets.iter().find_map(|sts| {
            sts.spec
                .as_ref()?
                .template
                .spec
                .as_ref()?
                .containers
                .iter()
                .find(|c| c.name == name)
        })
    }

    /// Whether a service of this name is in the plan.
    pub fn has_service(&self, name: &str) -> bool {
        self.services
            .iter()
            .any(|s| s.metadata.name.as_deref() == Some(name))
    }

    /// The rendered body of a single-file ConfigMap.
    pub fn config(&self, name: &str) -> Option<&str> {
        self.configs
            .iter()
            .find(|c| c.metadata.name.as_deref() == Some(name))
            .and_then(|c| c.data.as_ref())
            .and_then(|d| d.values().next())
            .map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Refusing to touch hand-managed node workloads
// ---------------------------------------------------------------------------

/// Namespaces where node, validator and MPC workloads are managed by hand or by
/// the legacy operator.
///
/// `lux-validators` is deliberately absent: it is the intended home for nodes
/// this operator does manage.
const RESERVED_NAMESPACES: [&str; 13] = [
    "lux-mainnet",
    "lux-testnet",
    "lux-devnet",
    "lux-system",
    "lux-network", // the legacy operator's own namespace
    "lux-mpc",
    "lux-mpc-keyset",
    "lux-mpc-keyset-backup",
    "lux-nodefleet",
    "zoo-mainnet",
    "zoo-testnet",
    "zoo-devnet",
    "kube-system",
];

/// Refuse any CR that could resolve onto a live hand-managed node.
///
/// The live Lux validators are one StatefulSet named `luxd`, pods `luxd-0..4`,
/// with PVCs `data-luxd-*`, managed outside this operator. Every apply here is
/// server-side with force, and every child is named after its CR — so a CR
/// named `luxd` in one of those namespaces would produce a StatefulSet that
/// takes ownership of the live one and binds its volumes. There is no undo for
/// that: the chain data is the asset.
///
/// This operator watches cluster-wide by default, which is the exact condition
/// that makes the collision reachable, so the refusal is per-CR rather than a
/// matter of how it happens to be scoped. It costs one comparison and rejects
/// nothing legitimate — nodes this operator manages are named for their org and
/// slot and live in their own namespace.
pub fn refuse_live_node(name: &str, namespace: &str) -> Result<()> {
    if name == "luxd" || name.starts_with("luxd-") {
        return Err(OperatorError::Config(format!(
            "refusing the name {name:?}: it is the live hand-managed luxd StatefulSet, and an apply here would take its volumes"
        )));
    }
    if RESERVED_NAMESPACES.contains(&namespace) {
        return Err(OperatorError::Config(format!(
            "refusing the namespace {namespace:?}: node and validator workloads there are managed outside this operator"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live validators are `luxd` / `luxd-0..4` with `data-luxd-*` volumes.
    /// A CR of any chain Kind carrying that name would build a StatefulSet that
    /// force-applies over them.
    #[test]
    fn the_live_validator_identity_is_refused() {
        for name in ["luxd", "luxd-0", "luxd-4", "luxd-anything"] {
            assert!(
                refuse_live_node(name, "chains").is_err(),
                "{name} must be refused"
            );
        }
        // A name that merely starts with the same letters is a different node.
        assert!(refuse_live_node("luxdiamond", "chains").is_ok());
        assert!(refuse_live_node("val-acme-0", "chains").is_ok());
    }

    /// Scoping is operator discipline; this backstops a misconfiguration,
    /// which matters because the default scope is the whole cluster.
    #[test]
    fn hand_managed_namespaces_are_refused() {
        for ns in ["lux-mainnet", "lux-network", "zoo-devnet", "kube-system"] {
            assert!(refuse_live_node("btc", ns).is_err(), "{ns} must be refused");
        }
        // The namespace meant for nodes this operator manages is not reserved.
        assert!(refuse_live_node("btc", "lux-validators").is_ok());
        assert!(refuse_live_node("btc", "chains").is_ok());
    }
}
