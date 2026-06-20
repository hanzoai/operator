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
//! Network, Ingress, DNS, BaseApp, SQL, KV, DocDB, IAM, KMS, LLM, S3, Chain,
//! Validator, Indexer, Explorer, SPA, Static, Queue, Observability, Function,
//! plus the unbranded Hanzo facades and the LuxRuntime + NodeFleet blockchain
//! Kinds.

use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::CustomResourceExt;
use operator::api_group::{ApiGroup, DEFAULT_API_GROUP};
use operator::crd::{
    BaseApp, Chain, Datastore, DocDB, Explorer, Function, Gateway, HanzoDNS, HanzoDatastore,
    HanzoService, Indexer, Ingress, LuxRuntime, Network, NodeFleet, Observability, Queue, Service,
    Static, Validator, DNS, IAM, KMS, KV, LLM, MPC, S3, SPA, SQL,
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
        BaseApp::crd(),
        SQL::crd(),
        KV::crd(),
        DocDB::crd(),
        IAM::crd(),
        KMS::crd(),
        LLM::crd(),
        S3::crd(),
        Chain::crd(),
        Validator::crd(),
        Indexer::crd(),
        Explorer::crd(),
        SPA::crd(),
        Static::crd(),
        Queue::crd(),
        Observability::crd(),
        Function::crd(),
        HanzoService::crd(),
        HanzoDatastore::crd(),
        HanzoDNS::crd(),
        LuxRuntime::crd(),
        NodeFleet::crd(),
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
    fn bundle_is_the_canonical_29_kind_set() {
        let crds = bundle(DEFAULT_API_GROUP);
        assert_eq!(crds.len(), 29, "managed Kind count must stay at 29");

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
