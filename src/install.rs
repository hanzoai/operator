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

use crate::api_group::DEFAULT_API_GROUP;
use crate::apply::{self, FIELD_MANAGER};
use crate::core::Result;
use crate::crd::{
    AgentDeployment, App, Base, Chain, Datastore, DocDB, Explorer, Function, Gateway, GitSource,
    ImageUpdate, Indexer, Ingress, LuxRuntime, ManagedDatabase, Network, NodeFleet, Observability,
    Queue, Service, Static, Validator, DNS, IAM, KMS, KV, LLM, MPC, S3, SPA, SQL,
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
        AgentDeployment::crd(),
        // The 29th Kind — the App-collapse super-facade. Emitted LAST so the
        // canonical Kind order (Service … AgentDeployment) is unchanged and App is
        // the additive tail.
        App::crd(),
        // Native GitOps Kinds — pull-sync (GitSource) + registry→git image
        // automation (ImageUpdate). Appended after App so they extend the tail
        // without disturbing the canonical order the checked-in bundles assert.
        GitSource::crd(),
        ImageUpdate::crd(),
    ];
    if group != DEFAULT_API_GROUP {
        for crd in &mut crds {
            rewrite_crd_group(crd, group);
        }
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

/// The `spec.role` enum VALUES, in the exact order the merged universe
/// `apps.hanzo.ai` CRD carries them. Injected onto the generated App CRD's
/// `role` property so the emitted schema agrees with the fleet CRD (schemars
/// models `role` only as an open string). Keep in lockstep with
/// `controllers::app::classify` — every value here MUST map to a profile there.
const APP_ROLE_ENUM: &[&str] = &[
    "generic",
    "service",
    "llm",
    "iam",
    "kms",
    "explorer",
    "function",
    "indexer",
    "observability",
    "queue",
    "datastore",
    "docdb",
    "kv",
    "s3",
    "sql",
    "managedDatabase",
    "base",
    "gateway",
    "ingress",
    "dns",
    "static",
    "spa",
    "mpc",
    "chain",
    "network",
    "nodeFleet",
    "luxRuntime",
    "validator",
    "agentDeployment",
];

/// Post-process the generated `App` CRD to match the merged universe
/// `apps.hanzo.ai` shape — the two things schemars cannot express:
/// (1) `x-kubernetes-preserve-unknown-fields: true` on `spec`, so the
///     role-specific fields (`AppSpec.extra`, `#[schemars(skip)]`) are carried
///     and never pruned — the exact data-loss bug that sank the reduced fork;
/// (2) the `spec.role` enum, so the emitted schema agrees with universe while the
///     Rust type stays an open string (so `classify` — not the schema — is the
///     runtime authority and an operator newer than the CRD still fails safe on
///     an unmodeled role).
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
        // (1) never prune the role-specific unknowns.
        spec.x_kubernetes_preserve_unknown_fields = Some(true);
        // (2) constrain role to the known profiles.
        if let Some(role) = spec.properties.as_mut().and_then(|p| p.get_mut("role")) {
            role.enum_ = Some(
                APP_ROLE_ENUM
                    .iter()
                    .map(|v| JSON(serde_json::Value::String((*v).to_string())))
                    .collect(),
            );
        }
    }
}

/// Rewrite a CRD's group (touches `spec.group` + `metadata.name = <plural>.<group>`).
pub fn rewrite_crd_group(crd: &mut CustomResourceDefinition, group: &str) {
    let plural = crd.spec.names.plural.clone();
    crd.spec.group = group.to_string();
    crd.metadata.name = Some(format!("{plural}.{group}"));
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

/// The operator ClusterRole. `group` is the CRD API group it manages (e.g.
/// `hanzo.ai`). Includes the grants the managed-upgrade FSM needs — pods +
/// persistentvolumeclaims (create/delete for pre-flight) and
/// `snapshot.storage.k8s.io` volumesnapshots (the CSI clone source).
pub fn operator_cluster_role(group: &str) -> ClusterRole {
    let rules = vec![
        // The managed CRDs + their status subresource.
        rule(&[group], &["*", "*/status"], ALL_VERBS),
        // KMSSecret children (kms.hanzo.ai).
        rule(&["kms.hanzo.ai"], &["kmssecrets"], ALL_VERBS),
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
    use super::*;

    // ---- CRD bundle (moved from generate_crd_yaml; the one home) ----

    #[test]
    fn bundle_is_the_canonical_30_kind_set() {
        let crds = crd_bundle(DEFAULT_API_GROUP);
        assert_eq!(
            crds.len(),
            31,
            "managed Kind count is 31 (28 canonical + App + the two native-GitOps Kinds)"
        );
        let kinds: Vec<&str> = crds.iter().map(|c| c.spec.names.kind.as_str()).collect();
        assert!(kinds.contains(&"Service"));
        assert!(kinds.contains(&"AgentDeployment"));
        assert!(kinds.contains(&"App"));
        assert!(kinds.contains(&"GitSource"));
        assert!(kinds.contains(&"ImageUpdate"));
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
    fn bundle_group_rewrite() {
        let crds = crd_bundle("lux.cloud");
        for crd in &crds {
            assert_eq!(crd.spec.group, "lux.cloud");
            let plural = &crd.spec.names.plural;
            assert_eq!(
                crd.metadata.name.as_deref(),
                Some(format!("{plural}.lux.cloud").as_str()),
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
