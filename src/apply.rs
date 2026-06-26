//! Idempotent server-side apply for managed K8s objects.
//!
//! Wraps `Api::patch` with `PatchParams::apply("hanzo-operator")` so each
//! reconcile is an SSA round — the operator owns its declared fields, and
//! anything edited out-of-band reverts on next loop.
//!
//! Two K8s fields fight plain SSA and need dedicated apply paths:
//!
//! * **`Service.spec.ports`** is a list-map keyed by `(port, protocol)`, so a
//!   changed port number for the same *named* port does not update in place —
//!   SSA appends a second entry and the apiserver rejects the duplicate port
//!   NAME (`spec.ports[1].name: Duplicate value: "http"`). [`apply_service`]
//!   replaces the ports array in place (keyed by name) before the SSA round.
//!
//! * **Immutable workload fields** (`Deployment.spec.selector`,
//!   `StatefulSet.spec.{selector,serviceName,volumeClaimTemplates}`) make SSA
//!   422 when a legacy object was created with a different shape.
//!   [`apply_deployment`] preserves the live selector in place (the pod
//!   template carries a superset of labels, so it keeps matching).
//!   [`apply_statefulset`] preserves the live volumeClaimTemplates ALWAYS — the
//!   PVC name is derived from the VCT name, so renaming one would strand the
//!   data — and, when an immutable field still drifts, performs a controlled
//!   recreate (orphan delete keeps Pods + PVCs) that reuses the same PVCs.

use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams, PropagationPolicy};
use kube::core::DynamicObject;
use kube::{Resource, ResourceExt};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::time::Duration;

use k8s_openapi::api::apps::v1::{Deployment, StatefulSet};
use k8s_openapi::api::core::v1::{Pod, Service, ServicePort};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use tracing::{info, warn};

use crate::core::{OperatorError, Result};

pub const FIELD_MANAGER: &str = "hanzo-operator";

/// Server-side apply a typed K8s object. Returns the live resource after
/// the apply. `obj` MUST have `metadata.name` set.
pub async fn apply<K>(api: &Api<K>, obj: &K) -> Result<K>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + Debug,
{
    let name =
        obj.meta().name.clone().ok_or_else(|| {
            crate::core::OperatorError::Config("apply: missing metadata.name".into())
        })?;
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    let out = api.patch(&name, &pp, &Patch::Apply(obj)).await?;
    Ok(out)
}

/// Apply a DynamicObject (used for CRDs whose types are not statically known,
/// like KMSSecret). `api` carries the ApiResource on its DynamicType.
pub async fn apply_dynamic(api: &Api<DynamicObject>, obj: &DynamicObject) -> Result<DynamicObject> {
    let name = obj.metadata.name.clone().ok_or_else(|| {
        crate::core::OperatorError::Config("apply_dynamic: missing metadata.name".into())
    })?;
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    let out = api.patch(&name, &pp, &Patch::Apply(obj)).await?;
    Ok(out)
}

/// True iff `err` is a 422 rejection of an immutable / non-updatable field
/// (the only error class the controlled-recreate path should swallow).
pub fn is_immutable_error(err: &OperatorError) -> bool {
    if let OperatorError::KubeApi(kube::Error::Api(resp)) = err {
        if resp.code == 422 {
            let m = resp.message.to_ascii_lowercase();
            return m.contains("immutable")
                || m.contains("are forbidden")
                || m.contains("may not change")
                || m.contains("updates to statefulset spec");
        }
    }
    false
}

/// SSA a `Service`, replacing its ports in place (keyed by NAME) so a changed
/// port number updates the existing entry instead of appending a duplicate.
///
/// `Service.spec.ports` is `x-kubernetes-list-type: map` keyed on
/// `(port, protocol)`, so SSA treats `8080/TCP` and `80/TCP` as distinct
/// entries and keeps both — yielding two ports named `http` and a
/// `Duplicate value: "http"` 422. A JSON merge patch (RFC 7386 replaces arrays
/// wholesale, ignoring the list-map key) rewrites the array to exactly the
/// desired set, dropping stale/foreign entries; the subsequent SSA round then
/// re-establishes operator ownership without re-appending.
pub async fn apply_service(api: &Api<Service>, svc: &Service) -> Result<Service> {
    let name = svc
        .meta()
        .name
        .clone()
        .ok_or_else(|| OperatorError::Config("apply_service: missing metadata.name".into()))?;

    if let Some(live) = api.get_opt(&name).await? {
        let desired_ports = svc
            .spec
            .as_ref()
            .and_then(|s| s.ports.clone())
            .unwrap_or_default();
        let live_ports = live
            .spec
            .as_ref()
            .and_then(|s| s.ports.clone())
            .unwrap_or_default();
        if !desired_ports.is_empty() && ports_differ(&live_ports, &desired_ports) {
            info!(service = %name, "replacing Service ports in place (avoids SSA dup-port append)");
            let pp = PatchParams {
                field_manager: Some(FIELD_MANAGER.to_string()),
                ..Default::default()
            };
            let patch = serde_json::json!({ "spec": { "ports": desired_ports } });
            api.patch(&name, &pp, &Patch::Merge(&patch)).await?;
        }
    }
    apply(api, svc).await
}

/// Compare two port lists as unordered sets of `(name, port, protocol,
/// targetPort)`, normalizing the protocol default (`TCP`) so an explicit-vs-
/// implicit protocol is not seen as drift.
fn ports_differ(live: &[ServicePort], desired: &[ServicePort]) -> bool {
    fn key(p: &ServicePort) -> (String, i32, String, Option<i32>) {
        let proto = p.protocol.clone().unwrap_or_else(|| "TCP".to_string());
        let tp = match &p.target_port {
            Some(IntOrString::Int(i)) => Some(*i),
            _ => None,
        };
        (p.name.clone().unwrap_or_default(), p.port, proto, tp)
    }
    let mut a: Vec<_> = live.iter().map(key).collect();
    let mut b: Vec<_> = desired.iter().map(key).collect();
    a.sort();
    b.sort();
    a != b
}

/// SSA a `Deployment`, preserving the LIVE `spec.selector` (immutable). Legacy
/// deployments use `{app: <name>}`; the operator's canonical selector is
/// `{name, instance}`. The pod template carries a superset of labels, so the
/// live selector keeps matching — we converge in place with zero disruption
/// rather than tripping the immutable-selector 422. The live selector's labels
/// are folded into the template so the template always satisfies the selector.
pub async fn apply_deployment(api: &Api<Deployment>, deploy: &Deployment) -> Result<Deployment> {
    let name = deploy
        .meta()
        .name
        .clone()
        .ok_or_else(|| OperatorError::Config("apply_deployment: missing metadata.name".into()))?;

    let mut d = deploy.clone();
    if let Some(live) = api.get_opt(&name).await? {
        if let (Some(ds), Some(ls)) = (d.spec.as_mut(), live.spec.as_ref()) {
            if ds.selector != ls.selector {
                info!(deployment = %name, "preserving live (immutable) Deployment selector");
            }
            ds.selector = ls.selector.clone();
            if let Some(ml) = ls.selector.match_labels.as_ref() {
                fold_into_template_labels(&mut ds.template, ml);
            }
        }
    }
    apply(api, &d).await
}

/// SSA a `StatefulSet` data-safely. The live `volumeClaimTemplates` are ALWAYS
/// preserved (the PVC name `<vct>-<sts>-<ordinal>` derives from the VCT name, so
/// a rename would bind a fresh empty PVC and strand the data). If an immutable
/// field (selector / serviceName) still drifts, a controlled recreate converges
/// it: orphan-delete the StatefulSet (Pods + PVCs survive), drop only the pods
/// the new selector can't adopt (releasing their RWO PVCs — deleting a pod never
/// deletes its PVC), then recreate so the same PVCs are re-bound.
pub async fn apply_statefulset(
    api: &Api<StatefulSet>,
    pods: &Api<Pod>,
    sts: &StatefulSet,
) -> Result<StatefulSet> {
    let name = sts
        .meta()
        .name
        .clone()
        .ok_or_else(|| OperatorError::Config("apply_statefulset: missing metadata.name".into()))?;

    let mut s = sts.clone();
    if let Some(live) = api.get_opt(&name).await? {
        if let (Some(ss), Some(ls)) = (s.spec.as_mut(), live.spec.as_ref()) {
            // DATA SAFETY: keep the live VCTs verbatim so the existing PVCs are
            // reused (and the container's data volumeMount keeps resolving).
            if ls.volume_claim_templates.is_some() {
                ss.volume_claim_templates = ls.volume_claim_templates.clone();
            }
        }
    }

    match apply(api, &s).await {
        Ok(out) => Ok(out),
        Err(e) if is_immutable_error(&e) => {
            warn!(statefulset = %name, error = %e, "immutable StatefulSet drift — controlled recreate (PVC-retained)");
            recreate_statefulset(api, pods, &s).await
        }
        Err(e) => Err(e),
    }
}

/// Orphan-delete + recreate a StatefulSet, retaining PVCs and only restarting
/// pods the new selector cannot adopt. `desired` must already carry the
/// preserved VCTs (see [`apply_statefulset`]).
async fn recreate_statefulset(
    api: &Api<StatefulSet>,
    pods: &Api<Pod>,
    desired: &StatefulSet,
) -> Result<StatefulSet> {
    let name = desired
        .meta()
        .name
        .clone()
        .ok_or_else(|| OperatorError::Config("recreate_statefulset: missing name".into()))?;

    // 1. Orphan-delete the StatefulSet object — Pods AND PVCs survive.
    let dp = DeleteParams {
        propagation_policy: Some(PropagationPolicy::Orphan),
        ..Default::default()
    };
    match api.delete(&name, &dp).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }

    // 2. Wait for the object to actually disappear before recreating.
    let mut gone = false;
    for _ in 0..60 {
        if api.get_opt(&name).await?.is_none() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    if !gone {
        return Err(OperatorError::Reconcile(format!(
            "timed out waiting for StatefulSet {name} to delete before recreate"
        )));
    }

    // 3. Drop only the orphaned ordinal pods the new selector can't adopt
    //    (those hold the ordinal name + the RWO PVC). Pods that DO match are
    //    adopted on recreate with no restart. PVCs are never touched here.
    let want = desired
        .spec
        .as_ref()
        .and_then(|s| s.selector.match_labels.clone())
        .unwrap_or_default();
    if let Ok(list) = pods.list(&ListParams::default()).await {
        for p in list.items {
            let pname = p.name_any();
            if !is_ordinal_pod(&pname, &name) {
                continue;
            }
            let labels = p.labels();
            let adoptable = want.iter().all(|(k, v)| labels.get(k) == Some(v));
            if !adoptable {
                info!(statefulset = %name, pod = %pname, "deleting unadoptable pod before recreate (PVC retained)");
                let _ = pods.delete(&pname, &DeleteParams::default()).await;
            }
        }
    }

    // 4. Recreate the StatefulSet — same VCTs ⇒ same PVCs re-bound.
    info!(statefulset = %name, "recreating StatefulSet with canonical spec (PVCs retained)");
    apply(api, desired).await
}

/// `<sts>-<n>` where `<n>` is a non-empty run of digits.
fn is_ordinal_pod(pod: &str, sts: &str) -> bool {
    pod.strip_prefix(&format!("{sts}-"))
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Ensure the pod template's labels contain every key/value in `selector` so
/// the template satisfies a preserved (immutable) selector.
fn fold_into_template_labels(
    template: &mut k8s_openapi::api::core::v1::PodTemplateSpec,
    selector: &BTreeMap<String, String>,
) {
    let meta = template
        .metadata
        .get_or_insert_with(Default::default);
    let labels = meta.labels.get_or_insert_with(Default::default);
    for (k, v) in selector {
        labels.entry(k.clone()).or_insert_with(|| v.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::PodTemplateSpec;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn sp(name: &str, port: i32, target: i32) -> ServicePort {
        ServicePort {
            name: Some(name.to_string()),
            port,
            target_port: Some(IntOrString::Int(target)),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn ports_differ_on_changed_port_same_name() {
        // The vmsingle bug: same name "http", port 80 -> 8428.
        let live = vec![sp("http", 8428, 8428)];
        let desired = vec![sp("http", 80, 8429)];
        assert!(ports_differ(&live, &desired));
    }

    #[test]
    fn ports_equal_ignoring_order_and_protocol_default() {
        let mut implicit = sp("http", 80, 8080);
        implicit.protocol = None; // implicit TCP
        let live = vec![sp("metrics", 9090, 9090), implicit];
        let desired = vec![sp("http", 80, 8080), sp("metrics", 9090, 9090)];
        assert!(!ports_differ(&live, &desired));
    }

    #[test]
    fn ports_differ_when_target_port_is_string_vs_int() {
        let mut named = sp("http", 8428, 0);
        named.target_port = Some(IntOrString::String("http".to_string()));
        let live = vec![named];
        let desired = vec![sp("http", 8428, 8428)];
        // String target normalizes to None, so it differs from Int(8428):
        // the merge patch then pins the numeric target. Same wire endpoint.
        assert!(ports_differ(&live, &desired));
    }

    #[test]
    fn is_ordinal_pod_matches_only_sts_ordinals() {
        assert!(is_ordinal_pod("kv-0", "kv"));
        assert!(is_ordinal_pod("kv-12", "kv"));
        assert!(!is_ordinal_pod("kv", "kv"));
        assert!(!is_ordinal_pod("kv-", "kv"));
        assert!(!is_ordinal_pod("kv-abc", "kv"));
        assert!(!is_ordinal_pod("kv-data-kv-0", "kv")); // a PVC-shaped name
        assert!(!is_ordinal_pod("other-0", "kv"));
    }

    #[test]
    fn fold_into_template_labels_adds_missing_keeps_existing() {
        let mut t = PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(BTreeMap::from([(
                    "app.kubernetes.io/name".to_string(),
                    "kv".to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut sel = BTreeMap::new();
        sel.insert("app".to_string(), "kv".to_string());
        sel.insert("app.kubernetes.io/name".to_string(), "WRONG".to_string());
        fold_into_template_labels(&mut t, &sel);
        let labels = t.metadata.unwrap().labels.unwrap();
        assert_eq!(labels.get("app").unwrap(), "kv"); // added
        assert_eq!(labels.get("app.kubernetes.io/name").unwrap(), "kv"); // kept (not clobbered)
    }
}
