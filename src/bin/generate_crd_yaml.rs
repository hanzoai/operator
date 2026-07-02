//! CRD YAML bundle generator.
//!
//! Emits one multi-document YAML stream carrying every CRD this operator
//! manages, at the requested API group. The compile-time group baked into the
//! `#[derive(CustomResource)]` macros is `hanzo.ai`; this binary rewrites
//! `spec.group` (and the derived `metadata.name = <plural>.<group>`) so the
//! same Rust types produce the per-universe bundles checked in under
//! `k8s/crds/all-<group>.yaml`.
//!
//! Usage:
//!
//! ```text
//! generate-crd-yaml [--api-group <group>]
//! ```
//!
//! `--api-group` (or `OPERATOR_API_GROUP`) defaults to `hanzo.ai`. Other
//! universes: `lux.cloud`, `zoo.cloud`, `osage.cloud`.
//!
//! The Kind set + ordering is canonical and mirrors `bootnode/operator`'s
//! `config/crd/bases/bootno.de_*.yaml`: Service, Datastore, Gateway, MPC,
//! Network, Ingress, DNS, Base, SQL, KV, DocDB, IAM, KMS, LLM, S3, Chain,
//! Validator, Indexer, Explorer, SPA, Static, Queue, Observability, Function,
//! ManagedDatabase, the LuxRuntime + NodeFleet blockchain Kinds, plus the
//! AgentDeployment (autonomous-bot) Kind. No compat aliases — the v1 Kinds are
//! the one way.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt;
use operator::api_group::{ApiGroup, DEFAULT_API_GROUP};
use operator::crd::{
    AgentDeployment, Base, Chain, Datastore, DocDB, Explorer, Function, Gateway, Indexer, Ingress,
    LuxRuntime, ManagedDatabase, Network, NodeFleet, Observability, Queue, Service, Static,
    Validator, DNS, IAM, KMS, KV, LLM, MPC, S3, SPA, SQL,
};

/// Returns every managed CRD in canonical bundle order, with the group already
/// rewritten to `group`. Kept as a function (not inline in `main`) so the unit
/// test can assert the full set without spawning the binary.
fn bundle(group: &str) -> Vec<CustomResourceDefinition> {
    // One closure per Kind so the turbofish-heavy `crd()` calls read cleanly.
    // Order is identical to `bootnode/operator` config/crd/bases and to the
    // checked-in `k8s/crds/all-*.yaml` bundles.
    let mut crds = vec![
        Service::crd(),
        Datastore::crd(),
        Gateway::crd(),
        MPC::crd(),
        Network::crd(),
        Ingress::crd(),
        DNS::crd(),
        Base::crd(),
        SQL::crd(),
        KV::crd(),
        DocDB::crd(),
        IAM::crd(),
        KMS::crd(),
        LLM::crd(),
        S3::crd(),
        ManagedDatabase::crd(),
        Chain::crd(),
        Validator::crd(),
        Indexer::crd(),
        Explorer::crd(),
        SPA::crd(),
        Static::crd(),
        Queue::crd(),
        Observability::crd(),
        Function::crd(),
        LuxRuntime::crd(),
        NodeFleet::crd(),
        AgentDeployment::crd(),
    ];

    if group != DEFAULT_API_GROUP {
        for crd in &mut crds {
            rewrite_group(crd, group);
        }
    }
    crds
}

/// Rewrites a single CRD's group from the compile-time default to `group`.
/// Touches `spec.group` and the derived `metadata.name = <plural>.<group>`
/// (the only two places the group string appears in a kube-rs-generated CRD).
fn rewrite_group(crd: &mut CustomResourceDefinition, group: &str) {
    let plural = crd.spec.names.plural.clone();
    crd.spec.group = group.to_string();
    crd.metadata.name = Some(format!("{plural}.{group}"));
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tiny hand-rolled flag parse — this binary takes exactly one optional
    // flag and pulling `clap` into a generator is unwarranted weight.
    let mut api_group: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--api-group" => {
                api_group = Some(args.next().ok_or("--api-group requires a value")?);
            }
            other if other.starts_with("--api-group=") => {
                api_group = Some(other["--api-group=".len()..].to_string());
            }
            "-h" | "--help" => {
                eprintln!("usage: generate-crd-yaml [--api-group <group>]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let group = ApiGroup::resolve(api_group.as_deref()).group;
    let mut out = String::new();
    for crd in bundle(&group) {
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&crd)?);
    }
    print!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_is_the_canonical_28_kind_set() {
        let crds = bundle(DEFAULT_API_GROUP);
        assert_eq!(crds.len(), 28, "managed Kind count must stay at 28");

        // AgentDeployment (the autonomous-bot Kind) is in the bundle.
        let kinds: Vec<&str> = crds.iter().map(|c| c.spec.names.kind.as_str()).collect();
        assert!(
            kinds.contains(&"AgentDeployment"),
            "AgentDeployment Kind must be present",
        );
        let ad = crds
            .iter()
            .find(|c| c.spec.names.kind == "AgentDeployment")
            .expect("AgentDeployment CRD");
        assert_eq!(ad.spec.names.plural, "agentdeployments");
        assert_eq!(
            ad.metadata.name.as_deref(),
            Some("agentdeployments.hanzo.ai")
        );
        // `bot` shortname — a Bot is what it materializes.
        assert!(ad
            .spec
            .names
            .short_names
            .as_deref()
            .map(|s| s.contains(&"bot".to_string()))
            .unwrap_or(false));

        // Every Kind carries the default group and a well-formed metadata.name.
        for crd in &crds {
            assert_eq!(crd.spec.group, DEFAULT_API_GROUP);
            let plural = &crd.spec.names.plural;
            assert_eq!(
                crd.metadata.name.as_deref(),
                Some(format!("{plural}.{DEFAULT_API_GROUP}").as_str()),
            );
        }
    }

    #[test]
    fn bundle_is_compat_free_and_base_is_bare_named() {
        let crds = bundle(DEFAULT_API_GROUP);
        let kinds: Vec<&str> = crds.iter().map(|c| c.spec.names.kind.as_str()).collect();

        // Bare `Base` — not the old `BaseApp` — at `bases.hanzo.ai`.
        assert!(kinds.contains(&"Base"), "Base Kind must be present");
        assert!(
            !kinds.contains(&"BaseApp"),
            "legacy BaseApp Kind must be gone (renamed to Base)",
        );
        let base = crds
            .iter()
            .find(|c| c.spec.names.kind == "Base")
            .expect("Base CRD");
        assert_eq!(base.spec.names.plural, "bases");
        assert_eq!(base.spec.names.singular.as_deref(), Some("base"));
        assert_eq!(base.metadata.name.as_deref(), Some("bases.hanzo.ai"));

        // NO Hanzo-prefixed compat alias Kinds survive.
        for k in &kinds {
            assert!(
                !k.starts_with("Hanzo"),
                "compat-free: no Hanzo-prefixed Kind allowed, found {k}",
            );
        }
        assert!(!kinds.contains(&"HanzoService"));
        assert!(!kinds.contains(&"HanzoDatastore"));
        assert!(!kinds.contains(&"HanzoDNS"));
    }

    #[test]
    fn luxruntime_is_present_and_lux_network_is_gone() {
        let crds = bundle(DEFAULT_API_GROUP);
        let kinds: Vec<&str> = crds.iter().map(|c| c.spec.names.kind.as_str()).collect();
        assert!(
            kinds.contains(&"LuxRuntime"),
            "LuxRuntime Kind must be present for bootnode parity",
        );
        assert!(
            !kinds.contains(&"LuxNetwork"),
            "legacy LuxNetwork Kind must not be emitted",
        );

        let lrt = crds
            .iter()
            .find(|c| c.spec.names.kind == "LuxRuntime")
            .expect("LuxRuntime CRD");
        assert_eq!(lrt.spec.names.plural, "luxruntimes");
        assert_eq!(lrt.spec.names.singular.as_deref(), Some("luxruntime"));
        assert_eq!(
            lrt.spec.names.short_names.as_deref(),
            Some(["lrt".to_string()].as_slice()),
            "LuxRuntime shortname must match bootnode canonical `lrt`",
        );
        assert_eq!(lrt.metadata.name.as_deref(), Some("luxruntimes.hanzo.ai"));
    }

    #[test]
    fn group_rewrite_touches_spec_group_and_metadata_name() {
        let crds = bundle("lux.cloud");
        for crd in &crds {
            assert_eq!(crd.spec.group, "lux.cloud");
            let plural = &crd.spec.names.plural;
            assert_eq!(
                crd.metadata.name.as_deref(),
                Some(format!("{plural}.lux.cloud").as_str()),
            );
        }
        // Spot-check the renamed Kind survives the rewrite.
        let lrt = crds
            .iter()
            .find(|c| c.spec.names.kind == "LuxRuntime")
            .expect("LuxRuntime CRD");
        assert_eq!(lrt.metadata.name.as_deref(), Some("luxruntimes.lux.cloud"));
    }
}
