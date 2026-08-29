//! `operator install` / `operator up` — the bootstrap seam.
//!
//! The operator installs, operates, and upgrades the platform. This module is
//! the INSTALL verb: it renders the derived CRDs and the operator's own
//! namespace / ServiceAccount / ClusterRole / ClusterRoleBinding / Deployment
//! and applies them (server-side) to the current-context cluster, then — for
//! `up` — applies the platform's own App CRs so the running operator brings the
//! whole stack up.
//!
//! Everything is derived from code (the `CustomResource` derives) and rendered
//! in-process, so there is no file dependency and no `kubectl` in the image.
//! Applies are server-side and idempotent, so `install`/`up` are safe to re-run.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, HTTPGetAction, Namespace, PodSpec, PodTemplateSpec, Probe,
    ServiceAccount,
};
use k8s_openapi::api::rbac::v1::{ClusterRole, ClusterRoleBinding, PolicyRule, RoleRef, Subject};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::{
    CustomResourceDefinition, JSON,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::api::{Api, Patch, PatchParams};
use kube::core::{DynamicObject, GroupVersionKind};
use kube::{Client, CustomResourceExt};

use crate::apply::{self, FIELD_MANAGER};
use crate::core::Result;
use crate::crd::{
    App, Base, Chain, Datastore, DocDB, Explorer, Function, Gateway,
    Indexer, Ingress, LuxRuntime, ManagedDatabase, Network, NodeFleet, Observability, Queue,
    Service, Static, Validator, DNS, IAM, KMS, KV, LLM, MPC, S3, SPA, SQL, KMSSecret,
    BitcoinRuntime, EthereumRuntime, SolanaRuntime,
};

/// Default operator image (pinned semver; the caller overrides at install time).
pub const DEFAULT_OPERATOR_IMAGE: &str = "ghcr.io/hanzoai/operator";

// ============================================================================
// CRD bundle — the ONE canonical home for the managed-Kind set, shared by
// `install` (applies them) and the `generate-crd-yaml` binary (prints them).
// ============================================================================

/// Every managed CRD in canonical order, with the group rewritten to `group`.
/// The compile-time group baked into the derives is `hanzo.ai`; this rewrites
/// `spec.group` + the derived `metadata.name` for the other universes.
pub fn crd_bundle(group: &str) -> Vec<CustomResourceDefinition> {
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
        // The 29th Kind — the App-collapse super-facade. Emitted LAST so the
        // canonical Kind order (Service … NodeFleet) is unchanged and App is
        // the additive tail.
        App::crd(),
        KMSSecret::crd(),
        // Chains outside the luxd family. Each is its own Kind because each is
        // its own shape; they answer at the fixed blockchain group like the
        // rest of the chain surface.
        BitcoinRuntime::crd(),
        EthereumRuntime::crd(),
        SolanaRuntime::crd(),
    ];
    // Unconditionally. The derive bakes a group at compile time and it is not
    // the answer for any universe — including the default one, where it was
    // right only by coincidence. Every Kind's group is computed from its family,
    // so the default universe takes the same path as every other and there is no
    // second path to be wrong in.
    for crd in &mut crds {
        rewrite_crd_group(crd, group);
    }
    // Harden the App CRD to the merged universe `apps.hanzo.ai` wire shape — the
    // two things schemars cannot express: `x-kubernetes-preserve-unknown-fields`
    // on `spec` (so the role-specific datastore/ingress fields are NEVER pruned)
    // + the `role` enum. Applied in the ONE bundle so both `install` and
    // `generate-crd-yaml` emit the hardened App CRD.
    for crd in &mut crds {
        if crd.spec.names.kind == "App" {
            harden_app_crd(crd);
        }
    }
    crds
}

/// The `spec.role` enum VALUES, projected from `controllers::app::ROLES`.
///
/// Schemars models `role` only as an open string, so the enum is injected onto
/// the generated App CRD. It is read off the dispatch table rather than listed
/// again here: the previous copy carried a comment asking that the two be kept
/// in lockstep, and by the time anyone checked, five of its values reached no
/// arm at all and one arm was missing from it.
fn app_role_enum() -> Vec<&'static str> {
    crate::controllers::app::ROLES
        .iter()
        .map(|(name, _)| *name)
        .collect()
}


/// The role-specific `AppSpec` fields that live on the wire but not in the Rust
/// struct: they ride in `AppSpec.extra` (`#[serde(flatten)]` + `#[schemars(skip)]`)
/// and so are invisible to schemars. They MUST be declared here — see
/// `harden_app_crd` for what happens when they are not.
///
/// Modelled from the live objects: `domains` carries domain/routes/tls/annotations
/// with an integer `servicePort`; `storage` carries
/// size/storageClassName/volumeName/retentionPolicy.
fn app_role_specific_properties() -> serde_json::Value {
    serde_json::json!({
        "clusterIssuer":     { "type": "string" },
        "credentialsSecret": { "type": "string" },
        "ingressClassName":  { "type": "string" },
        "tag":               { "type": "string" },
        "type":              { "type": "string" },
        "serviceAliases":    { "type": "array", "items": { "type": "string" } },
        "storage": { "type": "object", "properties": {
            "retentionPolicy":  { "type": "string" },
            "size":             { "type": "string" },
            "storageClassName": { "type": "string" },
            "volumeName":       { "type": "string" },
        }},
        "domains": { "type": "array", "items": { "type": "object", "properties": {
            "annotations": { "type": "object", "additionalProperties": { "type": "string" } },
            "domain":      { "type": "string" },
            "tls":         { "type": "boolean" },
            "routes": { "type": "array", "items": { "type": "object", "properties": {
                "path":        { "type": "string" },
                "pathType":    { "type": "string" },
                "serviceName": { "type": "string" },
                "servicePort": { "type": "integer" },
            }}},
        }}},
    })
}

/// Post-process the generated `App` CRD to match the `apps.hanzo.ai` shape the
/// fleet actually runs — the two things schemars cannot express:
///
/// (1) the role-specific spec fields, DECLARED rather than preserved-as-unknown.
///   `x-kubernetes-preserve-unknown-fields: true` on `spec` looks like the
///   obvious way to carry `AppSpec.extra`, and it does carry it — but it also
///   collapses the PUBLISHED OpenAPI model to ZERO spec properties. Hanzo CD
///   builds its structured-merge diff from that published model, not from the
///   CRD, so every comparison died on `.spec.image: field not declared in
///   schema`, sync went Unknown, and reconciliation silently stopped for EVERY
///   App CR in the fleet while the Application still reported Healthy at the
///   right revision. The flag was load-bearing only because eight fields were
///   in live use and undeclared, so dropping it alone would have PRUNED them
///   off 80 CRs. Declaring them first (`app_role_specific_properties`) and only
///   then dropping the flag is what makes the model complete AND lossless.
///   `AppSpec.extra` still collects them at the serde layer — that is
///   independent of the schema, so a DECLARED field is stored and projected
///   exactly as before.
///
///   This ran as a hand-edit on `k8s/crds/all-hanzo.ai.yaml` (f722cd8) that the
///   generator could not reproduce, so the next `generate-crd-yaml` would have
///   silently reverted a fleet-down fix. It lives in the generator now: the
///   bundles are generated output again, and regeneration is faithful.
///
/// (2) the `spec.role` enum, so the emitted schema agrees with universe while the
///   Rust type stays an open string (so `classify` — not the schema — is the
///   runtime authority and an operator newer than the CRD still fails safe on
///   an unmodeled role).
fn harden_app_crd(crd: &mut CustomResourceDefinition) {
    for version in &mut crd.spec.versions {
        let Some(schema) = version.schema.as_mut() else {
            continue;
        };
        let Some(root) = schema.open_api_v3_schema.as_mut() else {
            continue;
        };
        let Some(props) = root.properties.as_mut() else {
            continue;
        };
        let Some(spec) = props.get_mut("spec") else {
            continue;
        };
        // (1) declare the role-specific fields, and DO NOT set preserve-unknown:
        // a declared model is what the published OpenAPI model — and therefore
        // the control plane's diff — is built from.
        spec.x_kubernetes_preserve_unknown_fields = None;
        if let (Some(target), serde_json::Value::Object(extra)) =
            (spec.properties.as_mut(), app_role_specific_properties())
        {
            for (name, schema) in extra {
                match serde_json::from_value(schema) {
                    Ok(props) => {
                        target.insert(name, props);
                    }
                    // Unreachable for the literal above; a malformed declaration
                    // must never silently drop the field it was meant to protect.
                    Err(e) => panic!("App role-specific property {name} is not a schema: {e}"),
                }
            }
        }
        // (2) constrain role to the known profiles.
        if let Some(role) = spec.properties.as_mut().and_then(|p| p.get_mut("role")) {
            role.enum_ = Some(
                app_role_enum()
                    .iter()
                    .map(|v| JSON(serde_json::Value::String((*v).to_string())))
                    .collect(),
            );
        }
    }
}

/// Where a Kind's group comes from.
///
/// The group string used to carry two facts braided together — which family a
/// Kind belongs to, and what this universe is called — and `rewrite_crd_group`
/// recovered the first by parsing for a suffix. That is inference standing in
/// for a declaration: the family is a property of the KIND, the universe name is
/// a property of the DEPLOYMENT, and neither is knowable from the other.
///
/// So the family is a value, the universe is an argument, and the group is what
/// you get by applying one to the other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Follows the universe: `hanzo.ai`, `lux.cloud`.
    Universe,
    /// Named for the service that owns it and sits beside the universe:
    /// `kms.hanzo.ai`, `kms.lux.cloud`.
    Beside(&'static str),
    /// One group for every universe. lux/operator materializes children at
    /// `bootno.de` precisely so a single reader sees one canonical group;
    /// flipping it per universe is what would split that reader.
    Fixed(&'static str),
}

impl Family {
    /// The group this family resolves to in a given universe.
    pub fn group(self, universe: &str) -> String {
        match self {
            Family::Universe => universe.to_string(),
            Family::Beside(service) => format!("{service}.{universe}"),
            Family::Fixed(group) => group.to_string(),
        }
    }
}

/// The Kinds that answer at the fixed blockchain group.
pub const BLOCKCHAIN_KINDS: [&str; 11] = [
    "Network",
    "Chain",
    "Validator",
    "Indexer",
    "Explorer",
    "LuxRuntime",
    "NodeFleet",
    "MPC",
    "BitcoinRuntime",
    "EthereumRuntime",
    "SolanaRuntime",
];

/// Which family a Kind belongs to. One place, so the bundle and anything that
/// checks the bundle cannot disagree about it.
pub fn family_of(kind: &str) -> Family {
    if BLOCKCHAIN_KINDS.contains(&kind) {
        Family::Fixed("bootno.de")
    } else if kind == "KMSSecret" {
        Family::Beside("kms")
    } else {
        Family::Universe
    }
}

/// Set a CRD's group from its family (touches `spec.group` + `metadata.name`).
pub fn rewrite_crd_group(crd: &mut CustomResourceDefinition, universe: &str) {
    let group = family_of(&crd.spec.names.kind).group(universe);
    crd.metadata.name = Some(format!("{}.{}", crd.spec.names.plural, group));
    crd.spec.group = group;
}

// ============================================================================
// Operator self-install manifests — rendered in-code, pure + unit-tested.
// ============================================================================

/// Operator ServiceAccount name.
pub fn sa_name() -> String {
    "hanzo-operator".to_string()
}

/// Operator ClusterRole name.
pub fn cluster_role_name() -> String {
    "hanzo-operator".to_string()
}

fn rule(groups: &[&str], resources: &[&str], verbs: &[&str]) -> PolicyRule {
    PolicyRule {
        api_groups: Some(groups.iter().map(|s| s.to_string()).collect()),
        resources: Some(resources.iter().map(|s| s.to_string()).collect()),
        verbs: verbs.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

const ALL_VERBS: &[&str] = &[
    "get", "list", "watch", "create", "update", "patch", "delete",
];
const READ_VERBS: &[&str] = &["get", "list", "watch"];

/// Who reconciles each published Kind.
///
/// Publishing a CRD is a promise that something acts on the objects. Nothing
/// enforced that, so the estate drifted into 34 published Kinds of which 22 had
/// a reconciler here, six were reconciled in another repo, four stood aside for
/// a controller that does not exist, and two had nobody at all — and none of
/// that was visible without going and counting.
///
/// So every Kind names an owner and the test refuses a bundle where one does
/// not. A Kind may still be published with nothing reconciling it; what it may
/// not be is published by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// A controller this operator starts.
    Here(&'static str),
    /// Reconciled in another repo, named.
    Elsewhere(&'static str),
    /// Published and reconciled by nothing. The reason is the point.
    Unreconciled(&'static str),
}

/// The owner of every Kind in the bundle.
pub const OWNERS: &[(&str, Owner)] = &[
    // The workload surface. charts/app replaced these for the fleet — the
    // ApplicationSet says so outright — but the reconcilers still run, so a CR
    // written directly is still honoured.
    ("Service", Owner::Here("service")),
    ("LLM", Owner::Here("service")),
    ("IAM", Owner::Here("service")),
    ("KMS", Owner::Here("service")),
    ("Explorer", Owner::Here("service")),
    ("Function", Owner::Here("service")),
    ("Indexer", Owner::Here("service")),
    ("Observability", Owner::Here("service")),
    ("Queue", Owner::Here("service")),
    ("SPA", Owner::Here("service")),
    ("Static", Owner::Here("service")),
    ("Datastore", Owner::Here("datastore")),
    ("DocDB", Owner::Here("datastore")),
    ("S3", Owner::Here("datastore")),
    ("SQL", Owner::Here("sql")),
    ("KV", Owner::Here("kv")),
    ("Ingress", Owner::Here("ingress")),
    ("App", Owner::Here("app")),
    ("KMSSecret", Owner::Here("kms_zap")),
    ("BitcoinRuntime", Owner::Here("bitcoin")),
    ("EthereumRuntime", Owner::Here("ethereum")),
    ("SolanaRuntime", Owner::Here("solana")),
    // The chain surface at bootno.de. luxfi/operator's shim writes these as
    // children of its own lux.cloud Kinds, and hanzo-go/operator carries the
    // reconcilers. Published here because this operator installs the CRDs.
    ("Network", Owner::Elsewhere("hanzo-go/operator")),
    ("Chain", Owner::Elsewhere("hanzo-go/operator")),
    ("Validator", Owner::Elsewhere("hanzo-go/operator")),
    ("LuxRuntime", Owner::Elsewhere("hanzo-go/operator")),
    ("NodeFleet", Owner::Elsewhere("hanzo-go/operator")),
    ("MPC", Owner::Elsewhere("hanzo-go/operator")),
    // App's dispatch stands aside for a dedicated controller on these four, and
    // there is no dedicated controller. A CR of one is accepted and nothing
    // happens to it.
    ("Base", Owner::Unreconciled("App delegates to a controller that does not exist")),
    ("DNS", Owner::Unreconciled("App delegates to a controller that does not exist")),
    ("Gateway", Owner::Unreconciled("App delegates to a controller that does not exist")),
    ("ManagedDatabase", Owner::Unreconciled("App delegates to a controller that does not exist")),
];

/// The owner of `kind`, if the table names one.
pub fn owner_of(kind: &str) -> Option<Owner> {
    OWNERS.iter().find(|(k, _)| *k == kind).map(|(_, o)| *o)
}

/// Every API group this operator installs CRDs into, for `universe`.
///
/// Read off the bundle rather than listed by hand: the ClusterRole has to
/// cover exactly what the CRDs are published into, and a hand-kept list
/// silently stops covering it the moment a Kind is added. That already
/// happened twice — the whole blockchain family sits at the fixed
/// `bootno.de` group and was never granted, and the KMS grant was pinned to
/// `kms.hanzo.ai` while the controller reconciles at `kms.<universe>`.
///
/// Neither failed loudly. A reconciler with no grant does not crash, it just
/// never sees the object, and the CR waits forever with nothing in its status
/// to say why.
pub fn managed_groups(universe: &str) -> Vec<String> {
    let mut groups: Vec<String> = crd_bundle(universe)
        .iter()
        .map(|c| c.spec.group.clone())
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

/// The operator ClusterRole. `group` is the CRD API group it manages (e.g.
/// `hanzo.ai`). Includes the grants the managed-upgrade FSM needs — pods +
/// persistentvolumeclaims (create/delete for pre-flight) and
/// `snapshot.storage.k8s.io` volumesnapshots (the CSI clone source).
pub fn operator_cluster_role(group: &str) -> ClusterRole {
    // Every group the bundle publishes into: the universe's own, the KMS group
    // beside it, and the fixed blockchain group. Derived, so publishing a Kind
    // and granting it cannot come apart.
    let owned = managed_groups(group);
    let owned: Vec<&str> = owned.iter().map(String::as_str).collect();
    let rules = vec![
        // The managed CRDs + their status subresource.
        rule(&owned, &["*", "*/status"], ALL_VERBS),
        // Workloads the controllers materialize.
        rule(&["apps"], &["deployments", "statefulsets"], ALL_VERBS),
        rule(&["apps"], &["deployments/status"], READ_VERBS),
        // Core objects — includes pods + persistentvolumeclaims (pre-flight
        // clone) with create/delete, and events for reporting.
        rule(
            &[""],
            &[
                "services",
                "configmaps",
                "secrets",
                "persistentvolumeclaims",
                "pods",
                "serviceaccounts",
            ],
            ALL_VERBS,
        ),
        rule(&[""], &["events"], &["create", "patch"]),
        rule(&[""], &["namespaces"], READ_VERBS),
        // Networking, autoscaling, disruption budgets.
        rule(
            &["networking.k8s.io"],
            &["ingresses", "networkpolicies"],
            ALL_VERBS,
        ),
        rule(&["autoscaling"], &["horizontalpodautoscalers"], ALL_VERBS),
        rule(&["policy"], &["poddisruptionbudgets"], ALL_VERBS),
        // Pre-flight CSI snapshots (the migration-smoke data clone source).
        rule(
            &["snapshot.storage.k8s.io"],
            &["volumesnapshots"],
            ALL_VERBS,
        ),
        // Leader-election lease.
        rule(&["coordination.k8s.io"], &["leases"], ALL_VERBS),
    ];
    ClusterRole {
        metadata: ObjectMeta {
            name: Some(cluster_role_name()),
            labels: Some(operator_labels()),
            ..Default::default()
        },
        rules: Some(rules),
        ..Default::default()
    }
}

fn operator_labels() -> BTreeMap<String, String> {
    let mut l = BTreeMap::new();
    l.insert(
        "app.kubernetes.io/name".to_string(),
        "hanzo-operator".to_string(),
    );
    l.insert(
        "app.kubernetes.io/managed-by".to_string(),
        "hanzo-operator".to_string(),
    );
    l
}

pub fn operator_namespace_object(namespace: &str) -> Namespace {
    Namespace {
        metadata: ObjectMeta {
            name: Some(namespace.to_string()),
            labels: Some(operator_labels()),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn operator_service_account(namespace: &str) -> ServiceAccount {
    ServiceAccount {
        metadata: ObjectMeta {
            name: Some(sa_name()),
            namespace: Some(namespace.to_string()),
            labels: Some(operator_labels()),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn operator_cluster_role_binding(namespace: &str) -> ClusterRoleBinding {
    ClusterRoleBinding {
        metadata: ObjectMeta {
            name: Some(cluster_role_name()),
            labels: Some(operator_labels()),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "ClusterRole".to_string(),
            name: cluster_role_name(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: sa_name(),
            namespace: Some(namespace.to_string()),
            ..Default::default()
        }]),
    }
}

/// The operator's own Deployment. Runs `operator` (the reconcile loop) with the
/// resolved group/namespace, leader election on, and health/readiness probes on
/// `:8081`. `upgrade_fsm` sets `UPGRADE_FSM_ENABLED` (default off — a safe
/// drop-in first deploy).
pub fn operator_deployment(
    namespace: &str,
    image: &str,
    group: &str,
    upgrade_fsm: bool,
) -> Deployment {
    let env = vec![
        EnvVar {
            name: "OPERATOR_API_GROUP".to_string(),
            value: Some(group.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "OPERATOR_NAMESPACE".to_string(),
            value: Some(namespace.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "WATCH_NAMESPACE".to_string(),
            value: Some(String::new()), // all namespaces
            ..Default::default()
        },
        EnvVar {
            name: "LEADER_ELECT".to_string(),
            value: Some("true".to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "UPGRADE_FSM_ENABLED".to_string(),
            value: Some(upgrade_fsm.to_string()),
            ..Default::default()
        },
    ];
    let probe = |path: &str| Probe {
        http_get: Some(HTTPGetAction {
            path: Some(path.to_string()),
            port: IntOrString::Int(8081),
            ..Default::default()
        }),
        initial_delay_seconds: Some(5),
        period_seconds: Some(10),
        ..Default::default()
    };
    let container = Container {
        name: "operator".to_string(),
        image: Some(image.to_string()),
        args: Some(vec!["operator".to_string()]),
        env: Some(env),
        ports: Some(vec![ContainerPort {
            name: Some("health".to_string()),
            container_port: 8081,
            ..Default::default()
        }]),
        liveness_probe: Some(probe("/healthz")),
        readiness_probe: Some(probe("/readyz")),
        ..Default::default()
    };
    Deployment {
        metadata: ObjectMeta {
            name: Some("hanzo-operator".to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(operator_labels()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(operator_labels()),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(operator_labels()),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(sa_name()),
                    containers: vec![container],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        status: None,
    }
}

// ============================================================================
// Apply orchestration
// ============================================================================

/// Apply every derived CRD (server-side). Returns the count applied.
pub async fn install_crds(client: &Client, group: &str) -> Result<usize> {
    let api: Api<CustomResourceDefinition> = Api::all(client.clone());
    let bundle = crd_bundle(group);
    for crd in &bundle {
        apply::apply(&api, crd).await?;
    }
    Ok(bundle.len())
}

/// Apply the operator's own namespace + RBAC + Deployment (server-side).
pub async fn install_operator(
    client: &Client,
    namespace: &str,
    image: &str,
    group: &str,
    upgrade_fsm: bool,
) -> Result<()> {
    let ns_api: Api<Namespace> = Api::all(client.clone());
    apply::apply(&ns_api, &operator_namespace_object(namespace)).await?;

    let cr_api: Api<ClusterRole> = Api::all(client.clone());
    apply::apply(&cr_api, &operator_cluster_role(group)).await?;

    let crb_api: Api<ClusterRoleBinding> = Api::all(client.clone());
    apply::apply(&crb_api, &operator_cluster_role_binding(namespace)).await?;

    let sa_api: Api<ServiceAccount> = Api::namespaced(client.clone(), namespace);
    apply::apply(&sa_api, &operator_service_account(namespace)).await?;

    let dep_api: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    apply::apply(
        &dep_api,
        &operator_deployment(namespace, image, group, upgrade_fsm),
    )
    .await?;
    Ok(())
}

/// Apply every YAML document under `dir` (recursively, `*.yaml`/`*.yml`) as a
/// server-side apply. The platform's own App CRs — the stack the operator brings
/// up. Kind-agnostic: each doc's `apiVersion`/`kind` is resolved via discovery
/// (the same mechanism `kubectl` uses), so a `Service`/`SQL`/`Gateway` CR (and
/// core objects) all apply. Returns the count applied.
pub async fn apply_manifest_dir(client: &Client, dir: &Path) -> Result<usize> {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let entries = std::fs::read_dir(&path).map_err(|e| {
            crate::core::OperatorError::Config(format!("read manifests dir {path:?}: {e}"))
        })?;
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "yaml" && ext != "yml" {
                continue;
            }
            let text = std::fs::read_to_string(&p).map_err(|e| {
                crate::core::OperatorError::Config(format!("read manifest {p:?}: {e}"))
            })?;
            for doc in serde_yaml::Deserializer::from_str(&text) {
                let value = serde_yaml::Value::deserialize(doc)
                    .map_err(|e| crate::core::OperatorError::Config(format!("parse {p:?}: {e}")))?;
                if value.is_null() {
                    continue;
                }
                apply_yaml_value(client, value).await?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Apply a single parsed YAML document as a DynamicObject, resolving its
/// GroupVersionKind → plural via discovery.
async fn apply_yaml_value(client: &Client, value: serde_yaml::Value) -> Result<()> {
    let obj: DynamicObject = serde_yaml::from_value(value)
        .map_err(|e| crate::core::OperatorError::Config(format!("decode object: {e}")))?;
    let types = obj.types.as_ref().ok_or_else(|| {
        crate::core::OperatorError::Config("manifest doc missing apiVersion/kind".into())
    })?;
    let gvk = GroupVersionKind::try_from(types)
        .map_err(|e| crate::core::OperatorError::Config(format!("bad apiVersion/kind: {e}")))?;
    let (ar, caps) = kube::discovery::pinned_kind(client, &gvk)
        .await
        .map_err(crate::core::OperatorError::KubeApi)?;
    let api: Api<DynamicObject> = if caps.scope == kube::discovery::Scope::Namespaced {
        let ns = obj.metadata.namespace.as_deref().unwrap_or("default");
        Api::namespaced_with(client.clone(), ns, &ar)
    } else {
        Api::all_with(client.clone(), &ar)
    };
    let name =
        obj.metadata.name.clone().ok_or_else(|| {
            crate::core::OperatorError::Config("manifest doc missing name".into())
        })?;
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    api.patch(&name, &pp, &Patch::Apply(&obj))
        .await
        .map_err(crate::core::OperatorError::KubeApi)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::api_group::DEFAULT_API_GROUP;
    use super::*;

    // ---- CRD bundle (moved from generate_crd_yaml; the one home) ----

    /// The KMS family is named for the service that owns it, so its group sits
    /// BESIDE the universe rather than inside it: a lux install serves
    /// `kms.lux.cloud` while that same universe's own Kinds serve `lux.cloud`.
    /// The operator's internals stay `hanzo.ai` — only the rendered CRDs move.
    /// A runtime says WHICH node software it runs, and saying nothing still
    /// means luxd. The Kind is named for luxd for historical reasons, but the
    /// spec describes any chain node — so the engine has to be a value the CR
    /// carries rather than a fact the type asserts, and adding it must not
    /// invalidate a single CR written before it existed.
    /// A Kind that is published and not granted is invisible to its own
    /// reconciler, and invisible without an error: the watch simply returns
    /// nothing. So the ClusterRole must cover every group the bundle installs
    /// into, in every universe — not the universe's own group plus whatever was
    /// remembered by hand.
    /// Every role the CRD advertises must reach a profile, and every profile the
    /// controller has must be advertised. Both halves failed before the table
    /// was made the single source: `dns`, `managedDatabase`, `agentDeployment`,
    /// `luxRuntime` and `nodeFleet` were accepted at admission and then reached
    /// no arm, so those Apps requeued forever with nothing built and nothing
    /// said; `node` had an arm the schema rejected outright.
    /// kubectl resolves a short name across every group it knows, so a CRD that
    /// claims `svc` makes `kubectl get svc` ambiguous with core Services and
    /// the winner depends on discovery order. The Go operator claimed `svc`,
    /// `ing` and `ds` until it was brought into line with this set; the guard
    /// exists on both sides so neither drifts back.
    ///
    /// The same test refuses two Kinds claiming one short name, and two Kinds
    /// resolving to one CRD name — plural.group is the API server's key, so a
    /// collision there is one CRD quietly overwriting another.
    /// The CRD contract, checked.
    ///
    /// Both operators publish CRDs for the same Kinds into the same cluster, so
    /// a Kind, group, plural, short name or spec field that moves on one side
    /// alone means two CRDs for one concept — and the API server takes both,
    /// last write winning, without saying anything. crd-contract.json is the
    /// agreed shape and each side asserts its own output against its own copy,
    /// so neither can drift without a test going red in its own repo.
    /// Publishing a CRD promises something acts on the objects. Every Kind
    /// names who, and a Kind with no entry fails here — so a new one cannot be
    /// added without someone deciding what reconciles it, and an existing one
    /// cannot quietly lose its reconciler.
    #[test]
    fn every_published_kind_names_who_reconciles_it() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        for crd in &crds {
            let kind = &crd.spec.names.kind;
            assert!(
                owner_of(kind).is_some(),
                "{kind} is published and the owner table does not say what reconciles it"
            );
        }
        // And the table names nothing that is not published.
        for (kind, _) in OWNERS {
            assert!(
                crds.iter().any(|c| c.spec.names.kind == *kind),
                "the owner table names {kind}, which is not published"
            );
        }
        // A controller named as owning a Kind has to be one this operator
        // actually starts, or "Here" is a claim rather than a fact.
        let started = [
            "service", "datastore", "sql", "kv", "ingress", "app", "kms_zap",
            "bitcoin", "ethereum", "solana", "tenant", "upgrade",
        ];
        for (kind, owner) in OWNERS {
            if let Owner::Here(c) = owner {
                assert!(
                    started.contains(c),
                    "{kind} claims controller {c:?}, which this operator does not start"
                );
            }
        }
    }

    #[test]
    fn the_published_crds_match_the_contract() {
        let raw = include_str!("../crd-contract.json");
        let contract: serde_json::Value =
            serde_json::from_str(raw).expect("crd-contract.json parses");
        let expected = contract["kinds"].as_object().expect("kinds is an object");

        let crds = crd_bundle(DEFAULT_API_GROUP);
        assert_eq!(
            crds.len(),
            expected.len(),
            "the bundle publishes {} Kinds, the contract names {}",
            crds.len(),
            expected.len()
        );

        for crd in &crds {
            let kind = &crd.spec.names.kind;
            let want = expected
                .get(kind)
                .unwrap_or_else(|| panic!("{kind} is published and not in the contract"));

            let family = match family_of(kind) {
                Family::Universe => "universe",
                Family::Beside(_) => "kms",
                Family::Fixed(_) => "chain",
            };
            assert_eq!(want["family"], family, "{kind} family");
            assert_eq!(want["plural"], crd.spec.names.plural, "{kind} plural");

            let mut short = crd.spec.names.short_names.clone().unwrap_or_default();
            short.sort();
            let want_short: Vec<String> = want["shortNames"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(short, want_short, "{kind} short names");

            let mut fields: Vec<String> = crd.spec.versions[0]
                .schema
                .as_ref()
                .and_then(|s| s.open_api_v3_schema.as_ref())
                .and_then(|s| s.properties.as_ref())
                .and_then(|p| p.get("spec"))
                .and_then(|s| s.properties.as_ref())
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default();
            fields.sort();
            let want_fields: Vec<String> = want["specFields"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect();
            assert_eq!(
                fields, want_fields,
                "{kind} spec fields drifted from the contract"
            );
        }
    }

    #[test]
    fn short_names_and_crd_names_are_unambiguous() {
        const BUILTIN: [(&str, &str); 14] = [
            ("svc", "Service"), ("ing", "Ingress"), ("ds", "DaemonSet"),
            ("no", "Node"), ("po", "Pod"), ("deploy", "Deployment"),
            ("sts", "StatefulSet"), ("cm", "ConfigMap"), ("pvc", "PersistentVolumeClaim"),
            ("ns", "Namespace"), ("sa", "ServiceAccount"), ("rs", "ReplicaSet"),
            ("netpol", "NetworkPolicy"), ("pv", "PersistentVolume"),
        ];
        for universe in ["hanzo.ai", "lux.cloud"] {
            let crds = crd_bundle(universe);
            let mut short: std::collections::BTreeMap<String, String> = Default::default();
            let mut names: std::collections::BTreeMap<String, String> = Default::default();
            for crd in &crds {
                let kind = crd.spec.names.kind.clone();
                for s in crd.spec.names.short_names.clone().unwrap_or_default() {
                    if let Some((_, core)) = BUILTIN.iter().find(|(b, _)| *b == s) {
                        panic!("{kind} claims short name {s:?}, which is core {core}");
                    }
                    if let Some(prev) = short.insert(s.clone(), kind.clone()) {
                        panic!("{prev} and {kind} both claim short name {s:?}");
                    }
                }
                let full = format!("{}.{}", crd.spec.names.plural, crd.spec.group);
                if let Some(prev) = names.insert(full.clone(), kind.clone()) {
                    panic!("{prev} and {kind} both resolve to {full}");
                }
            }
            assert_eq!(names.len(), crds.len(), "{universe}: every CRD name is distinct");
        }
    }

    #[test]
    fn every_advertised_role_reaches_a_profile() {
        use crate::controllers::app::{classify, Dispatch, ROLES};

        for role in app_role_enum() {
            assert_ne!(
                classify(Some(role)),
                Dispatch::Unknown,
                "the CRD advertises role {role:?} and nothing reconciles it"
            );
        }
        // And the schema is the table, so a handled role cannot be missing from
        // it — the direction that made `node` unwritable.
        let advertised = app_role_enum();
        for (role, _) in ROLES {
            assert!(
                advertised.contains(role),
                "{role:?} is handled but not advertised; admission would reject it"
            );
        }
        // An unknown role still reaches the fail-safe rather than a panic.
        assert_eq!(classify(Some("no-such-role")), Dispatch::Unknown);
        assert_eq!(classify(None), Dispatch::Service);
    }

    #[test]
    fn every_group_the_bundle_publishes_is_granted() {
        for universe in ["hanzo.ai", "lux.cloud", "zoo.cloud"] {
            let role = operator_cluster_role(universe);
            let granted: Vec<String> = role
                .rules
                .unwrap_or_default()
                .iter()
                .filter(|r| r.resources.as_ref().is_some_and(|res| res.iter().any(|x| x == "*")))
                .flat_map(|r| r.api_groups.clone().unwrap_or_default())
                .collect();

            for crd in crd_bundle(universe) {
                assert!(
                    granted.contains(&crd.spec.group),
                    "{universe}: {} is published into {} and nothing grants it",
                    crd.spec.names.kind,
                    crd.spec.group
                );
            }
            // The three families are all really there, so this cannot pass by
            // granting one group that happens to cover the bundle.
            for expect in [universe, &format!("kms.{universe}"), "bootno.de"] {
                assert!(
                    granted.iter().any(|g| g == expect),
                    "{universe}: expected a grant on {expect}, got {granted:?}"
                );
            }
        }
    }

    #[test]
    fn a_runtime_names_its_engine_and_defaults_to_luxd() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        let rt = crds
            .iter()
            .find(|c| c.spec.names.kind == "LuxRuntime")
            .expect("LuxRuntime is in the bundle");
        let schema = rt.spec.versions[0]
            .schema
            .as_ref()
            .and_then(|s| s.open_api_v3_schema.as_ref())
            .expect("the CRD carries a schema");
        let spec = schema
            .properties
            .as_ref()
            .and_then(|p| p.get("spec"))
            .expect("spec is described");
        let props = spec.properties.as_ref().expect("spec has properties");

        assert!(
            props.contains_key("engine"),
            "a runtime must be able to name which fork it runs"
        );
        // Additive: requiring it would invalidate every CR written before it.
        if let Some(req) = spec.required.as_ref() {
            assert!(
                !req.iter().any(|r| r == "engine"),
                "engine must stay optional — requiring it rejects every CR written before it"
            );
        }
        // And there is no opaque escape hatch beside it. A chain of a different
        // shape gets its own Kind with its own real fields; a map the API server
        // cannot check would make that choice avoidable, and then optional.
        assert!(
            !props.contains_key("engineConfig"),
            "a foreign chain gets a Kind, not a blob"
        );
    }

    #[test]
    fn the_blockchain_family_is_fixed_across_universes() {
        let mut seen = Vec::new();
        for universe in ["hanzo.ai", "lux.cloud", "zoo.cloud", "osage.cloud"] {
            let crds = crd_bundle(universe);
            let mut groups: Vec<String> = crds
                .iter()
                .filter(|c| BLOCKCHAIN_KINDS.contains(&c.spec.names.kind.as_str()))
                .map(|c| c.spec.group.clone())
                .collect();
            groups.sort();
            groups.dedup();
            assert_eq!(
                groups,
                vec!["bootno.de".to_string()],
                "under {universe} the blockchain Kinds must all stay at bootno.de"
            );
            let n = crds
                .iter()
                .filter(|c| BLOCKCHAIN_KINDS.contains(&c.spec.names.kind.as_str()))
                .count();
            seen.push(n);
        }
        assert_eq!(seen, vec![11, 11, 11, 11], "the same 11 Kinds in every universe");
    }

    #[test]
    fn the_kms_group_keeps_its_prefix_across_universes() {
        for (universe, kms, own) in [
            ("hanzo.ai", "kms.hanzo.ai", "hanzo.ai"),
            ("lux.cloud", "kms.lux.cloud", "lux.cloud"),
            ("zoo.cloud", "kms.zoo.cloud", "zoo.cloud"),
        ] {
            let crds = crd_bundle(universe);
            let secret = crds
                .iter()
                .find(|c| c.spec.names.kind == "KMSSecret")
                .expect("the bundle installs the KMSSecret CRD");
            assert_eq!(
                secret.spec.group, kms,
                "under {universe} the KMS family must be {kms}, not folded into the universe group"
            );
            assert_eq!(
                secret.metadata.name.as_deref(),
                Some(format!("kmssecrets.{kms}").as_str()),
                "the CRD name follows its group"
            );
            let gateway = crds
                .iter()
                .find(|c| c.spec.names.kind == "Gateway")
                .expect("Gateway is in the bundle");
            assert_eq!(
                gateway.spec.group, own,
                "an ordinary Kind still lands in the universe's own group"
            );
        }
    }

    #[test]
    fn bundle_is_the_canonical_32_kind_set() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        assert_eq!(
            crds.len(),
            32,
            "managed Kind count is 32 (28 canonical + App + KMSSecret + the three foreign chain runtimes, less AgentDeployment and ImageUpdate, which nothing reconciles)"
        );
        let kinds: Vec<&str> = crds.iter().map(|c| c.spec.names.kind.as_str()).collect();
        assert!(kinds.contains(&"Service"));
        assert!(kinds.contains(&"App"));
        assert!(kinds.contains(&"KMSSecret"));
        for crd in &crds {
            let expected = family_of(&crd.spec.names.kind).group(DEFAULT_API_GROUP);
            assert_eq!(crd.spec.group, expected, "{} landed in the wrong family", crd.spec.names.kind);
            let plural = &crd.spec.names.plural;
            assert_eq!(
                crd.metadata.name.as_deref(),
                Some(format!("{plural}.{expected}").as_str()),
            );
        }
    }

    #[test]
    fn bundle_group_rewrite() {
        let crds = crd_bundle("lux.cloud");
        for crd in &crds {
            let expected = family_of(&crd.spec.names.kind).group("lux.cloud");
            assert_eq!(crd.spec.group, expected, "{} landed in the wrong family", crd.spec.names.kind);
            let plural = &crd.spec.names.plural;
            assert_eq!(
                crd.metadata.name.as_deref(),
                Some(format!("{plural}.{expected}").as_str()),
            );
        }
    }

    /// The Service CRD carries the new upgrade fields — proof `install` ships a
    /// schema that accepts `spec.upgradePolicy` + `status.upgrade`.
    #[test]
    fn service_crd_schema_carries_upgrade_fields() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        let svc = crds
            .iter()
            .find(|c| c.spec.names.kind == "Service")
            .expect("Service CRD");
        let schema = serde_json::to_string(&svc.spec.versions[0].schema).unwrap();
        assert!(
            schema.contains("upgradePolicy"),
            "spec.upgradePolicy must be in the schema"
        );
        assert!(
            schema.contains("lastGoodImage"),
            "status.lastGoodImage must be in the schema"
        );
        assert!(
            schema.contains("upgradeHistory"),
            "status.upgradeHistory must be in the schema"
        );
    }

    /// The Service/App CRDs AND the six DBSpec-backed datastore CRDs carry the
    /// securityContext passthrough + enableServiceLinks — proof `install`/
    /// `generate-crd-yaml` ship a schema that accepts them (an unmodeled field
    /// would be pruned by the apiserver). App flattens `ServiceSpec`; the
    /// datastore CRDs model `DBSpec`, so all must carry the fields for a hardened
    /// datastore App's projected spec to be stored rather than silently dropped.
    #[test]
    fn crd_schemas_carry_security_context_fields() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        // Service/App carry the fields on the ServiceSpec; the six DBSpec-backed
        // datastore CRDs carry them too, so a hardened datastore App's projected
        // spec is STORED (not pruned) and the standalone datastore CRs accept it.
        for kind in [
            "Service",
            "App",
            "SQL",
            "KV",
            "DocDB",
            "S3",
            "Datastore",
            "ManagedDatabase",
        ] {
            let crd = crds
                .iter()
                .find(|c| c.spec.names.kind == kind)
                .unwrap_or_else(|| panic!("{kind} CRD"));
            let schema = serde_json::to_string(&crd.spec.versions[0].schema).unwrap();
            assert!(
                schema.contains("securityContext"),
                "{kind}: spec.securityContext must be in the schema"
            );
            assert!(
                schema.contains("containerSecurityContext"),
                "{kind}: spec.containerSecurityContext must be in the schema"
            );
            assert!(
                schema.contains("enableServiceLinks"),
                "{kind}: spec.enableServiceLinks must be in the schema"
            );
            // The nested pod/container hardening shape survives into the schema.
            assert!(
                schema.contains("readOnlyRootFilesystem") && schema.contains("seccompProfile"),
                "{kind}: nested container/pod security fields must be modeled"
            );
            // A field the schema does not model is SILENTLY DROPPED on write, so
            // the CR author gets no error telling them why it never applied — a
            // field that exists in the Rust type but not in the structural schema
            // is unreachable. Modeling it in the schema IS the feature.
            assert!(
                schema.contains("fsGroupChangePolicy"),
                "{kind}: spec.securityContext.fsGroupChangePolicy must be in the schema"
            );
            assert!(
                schema.contains("OnRootMismatch"),
                "{kind}: the policy is a closed enum — a typo must be refused at admission"
            );
        }
    }

    /// Placement is modeled on the workload Kinds, so a CR can say WHERE it runs
    /// instead of encoding that intent as an inflated `resources.requests`.
    #[test]
    fn crd_schemas_carry_placement_fields() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        for kind in ["Service", "App"] {
            let crd = crds
                .iter()
                .find(|c| c.spec.names.kind == kind)
                .unwrap_or_else(|| panic!("{kind} CRD"));
            let schema = serde_json::to_string(&crd.spec.versions[0].schema).unwrap();
            for field in ["nodeSelector", "tolerations", "priorityClassName"] {
                assert!(
                    schema.contains(field),
                    "{kind}: spec.{field} must be in the schema"
                );
            }
            // The toleration shape survives, so a CR can express a real dedicated
            // pool rather than only a node label.
            assert!(
                schema.contains("tolerationSeconds") && schema.contains("effect"),
                "{kind}: the nested Toleration shape must be modeled"
            );
        }
    }

    /// Additive-only: the API version is unchanged. Bumping it would orphan the
    /// live CRs, so this locks the one thing that must never drift here.
    #[test]
    fn placement_is_additive_on_the_existing_v1() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        let app = crds.iter().find(|c| c.spec.names.kind == "App").unwrap();
        assert_eq!(app.spec.versions.len(), 1, "exactly one served version");
        assert_eq!(
            app.spec.versions[0].name, "v1",
            "placement must land on v1 — a version bump orphans every live CR"
        );
        assert!(app.spec.versions[0].served);
        assert!(app.spec.versions[0].storage);
    }

    /// The App spec must be DECLARED, not preserved-as-unknown.
    /// `x-kubernetes-preserve-unknown-fields` on `spec` collapses the published
    /// OpenAPI model to zero properties, which is what made Hanzo CD's diff die on
    /// `.spec.image: field not declared in schema` and stop reconciling every App
    /// CR in the fleet. The eight role-specific fields must be declared so that
    /// dropping the flag prunes nothing off the live CRs.
    #[test]
    fn app_crd_declares_the_role_specific_fields_instead_of_preserving_unknowns() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        let app = crds.iter().find(|c| c.spec.names.kind == "App").unwrap();
        let spec = app.spec.versions[0]
            .schema
            .as_ref()
            .and_then(|s| s.open_api_v3_schema.as_ref())
            .and_then(|r| r.properties.as_ref())
            .and_then(|p| p.get("spec"))
            .expect("App spec schema");

        assert_eq!(
            spec.x_kubernetes_preserve_unknown_fields, None,
            "preserve-unknown on spec zeroes the PUBLISHED model — the control \
             plane diffs against that, not the CRD"
        );
        let props = spec.properties.as_ref().expect("spec properties");
        // The field the CD comparison actually died on.
        assert!(props.contains_key("image"), "spec.image must be declared");
        // Every role-specific field that rides in `AppSpec.extra`. Undeclared +
        // no preserve-unknown would PRUNE these off the live CRs.
        for field in [
            "tag",
            "clusterIssuer",
            "domains",
            "ingressClassName",
            "credentialsSecret",
            "serviceAliases",
            "storage",
            "type",
        ] {
            assert!(
                props.contains_key(field),
                "spec.{field} is in live use — declaring it is what makes dropping \
                 preserve-unknown lossless"
            );
        }
    }

    // ---- operator manifests ----

    #[test]
    fn cluster_role_grants_preflight_resources() {
        let cr = operator_cluster_role(DEFAULT_API_GROUP);
        let rules = cr.rules.unwrap();
        let grants = |groups: &[&str], res: &str, verb: &str| {
            rules.iter().any(|r| {
                r.api_groups
                    .as_ref()
                    .map(|g| groups.iter().all(|x| g.contains(&x.to_string())))
                    .unwrap_or(false)
                    && r.resources
                        .as_ref()
                        .map(|rs| rs.contains(&res.to_string()))
                        .unwrap_or(false)
                    && r.verbs.contains(&verb.to_string())
            })
        };
        // Pre-flight needs: pods create/delete, pvc create/delete, volumesnapshots.
        assert!(grants(&[""], "pods", "create"));
        assert!(grants(&[""], "pods", "delete"));
        assert!(grants(&[""], "persistentvolumeclaims", "create"));
        assert!(grants(
            &["snapshot.storage.k8s.io"],
            "volumesnapshots",
            "delete"
        ));
        // The deny-egress NetworkPolicy scoping the pre-flight pod (HIGH-1) needs
        // networkpolicies create/delete (already covered by the Service NP grant).
        assert!(grants(&["networking.k8s.io"], "networkpolicies", "create"));
        assert!(grants(&["networking.k8s.io"], "networkpolicies", "delete"));
        // Plus the leader-election lease + the managed CRDs.
        assert!(grants(&["coordination.k8s.io"], "leases", "update"));
        assert!(grants(&[DEFAULT_API_GROUP], "*", "patch"));
    }

    #[test]
    fn operator_deployment_runs_the_reconcile_loop() {
        let d = operator_deployment(
            "hanzo-operator-system",
            "ghcr.io/hanzoai/operator:v0.7.0",
            "hanzo.ai",
            false,
        );
        let spec = d.spec.unwrap();
        assert_eq!(spec.replicas, Some(1));
        let pod = spec.template.spec.unwrap();
        assert_eq!(pod.service_account_name.as_deref(), Some("hanzo-operator"));
        let c = &pod.containers[0];
        assert_eq!(c.args.as_ref().unwrap(), &vec!["operator".to_string()]);
        // Health probes wired.
        assert!(c.liveness_probe.is_some() && c.readiness_probe.is_some());
        // FSM gate defaults off in the rendered Deployment.
        let fsm = c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .find(|e| e.name == "UPGRADE_FSM_ENABLED")
            .unwrap();
        assert_eq!(fsm.value.as_deref(), Some("false"));
    }

    #[test]
    fn cluster_role_binding_targets_the_operator_sa() {
        let crb = operator_cluster_role_binding("hanzo-operator-system");
        assert_eq!(crb.role_ref.name, "hanzo-operator");
        let subj = &crb.subjects.unwrap()[0];
        assert_eq!(subj.kind, "ServiceAccount");
        assert_eq!(subj.name, "hanzo-operator");
        assert_eq!(subj.namespace.as_deref(), Some("hanzo-operator-system"));
    }
}
