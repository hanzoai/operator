//! Shared helpers for the foreign-chain runtimes — bitcoind, Ethereum, Solana.
//!
//! One home rather than three copies. These are the pieces a chain node needs
//! and the cloud workloads do not: a credential projected out of a Secret, a
//! keypair mounted as a directory, a readiness check that is a open TCP port
//! rather than an HTTP route, because a syncing node answers a socket long
//! before it answers a request.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    EnvVarSource, LocalObjectReference, Probe, SecretKeySelector, SecretVolumeSource,
    TCPSocketAction, Volume,
};
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
