//! Idempotent server-side apply for managed K8s objects.
//!
//! Wraps `Api::patch` with `PatchParams::apply("hanzo-operator")` so each
//! reconcile is an SSA round — the operator owns its declared fields, and
//! anything edited out-of-band reverts on next loop.

use k8s_openapi::api::core::v1::{ConfigMap, Service};
use kube::api::{Api, DeleteParams, Patch, PatchParams};
use kube::core::DynamicObject;
use kube::Resource;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Debug;

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

/// Server-side apply a core v1 Service — the ONE way the operator writes a
/// Service. A Service's clusterIP is its stable identity: the immutable,
/// apiserver-assigned address every Endpoints/kube-proxy/DNS record resolves to.
/// Adopting a Service across a controller handoff (legacy `Service` CR → `App`)
/// must PRESERVE that value — same name ⇒ SSA merges the new ownerRef/labels
/// while the apiserver keeps the live clusterIP. A Service is therefore ADOPTED
/// IN PLACE, never delete+recreated: a recreate mints a fresh clusterIP and
/// strands every Endpoint/DNS record for ~50s while they re-propagate. There is
/// deliberately no `apply_or_recreate` counterpart for Services.
///
/// Fail-secure: an apiserver-assigned clusterIP present in the DESIRED object is
/// scrubbed before the apply (see [`scrub_assigned_cluster_ip`]), so SSA never
/// sends the immutable field even if a builder regresses and hard-codes one —
/// the apiserver stays the sole owner of the live value. The headless `"None"`
/// sentinel is a declared shape, not an assigned address, so it is preserved.
pub async fn apply_service(api: &Api<Service>, svc: &Service) -> Result<Service> {
    let mut desired = svc.clone();
    scrub_assigned_cluster_ip(&mut desired);
    apply(api, &desired).await
}

/// Drop an apiserver-ASSIGNED clusterIP (and the parallel `clusterIPs`) from a
/// desired Service so a server-side apply never sets or changes the immutable
/// field — the apiserver keeps the live value and adoption preserves identity.
/// The headless sentinel `spec.clusterIP: "None"` is a declared shape rather than
/// an assigned address, so it is KEPT (it must be sent for the Service to stay
/// headless). Pure over the object, so "adopt preserves clusterIP" is unit-
/// testable without a cluster.
pub(crate) fn scrub_assigned_cluster_ip(svc: &mut Service) {
    let Some(spec) = svc.spec.as_mut() else {
        return;
    };
    // A headless Service's identity IS clusterIP:None — a declared shape the
    // apply must carry, never an assigned address to preserve.
    if spec.cluster_ip.as_deref() == Some("None") {
        return;
    }
    // Any concrete address is apiserver-owned: leave it out of the desired state
    // so SSA cannot touch the immutable field.
    spec.cluster_ip = None;
    spec.cluster_ips = None;
}

/// Server-side apply a ConfigMap, refusing to clobber a populated ConfigMap
/// with empty data.
///
/// A ConfigMap whose `data` AND `binaryData` are both empty carries no config.
/// Force-applying it strips every key the `hanzo-operator` field manager owns,
/// blanking the workload's mounted config file → CrashLoopBackOff. An empty CR
/// config source is enough to produce one, and the consumer then dies on a file
/// it can open but not read. The
/// operator NEVER emits an empty ConfigMap: we skip the apply and leave any
/// existing content untouched. Skipping is also correct on first create — an
/// empty ConfigMap has no legitimate use. Returns `true` when applied, `false`
/// when skipped.
pub async fn apply_configmap(api: &Api<ConfigMap>, cm: &ConfigMap) -> Result<bool> {
    let name = cm.metadata.name.clone().ok_or_else(|| {
        crate::core::OperatorError::Config("apply_configmap: missing metadata.name".into())
    })?;
    if crate::manifests::configmap_is_empty(cm) {
        tracing::warn!(
            configmap = %name,
            namespace = cm.metadata.namespace.as_deref().unwrap_or_default(),
            "refusing to apply empty-data ConfigMap; leaving any existing content untouched"
        );
        return Ok(false);
    }
    apply(api, cm).await?;
    Ok(true)
}

/// Server-side apply, escalating to delete+recreate when the LIVE child holds a
/// structural conflict that SSA-merge cannot reconcile.
///
/// While adopting objects a raw manifest / ArgoCD created, the apply is
/// rejected with HTTP 422 (`Invalid`) in two situations:
///   * a stale field SSA cannot clear — a server-defaulted `emptyDir` left over
///     from a past source-less volume, or a duplicate probe handler — surfaces
///     as `may not specify more than 1 {volume,handler} type`;
///   * an immutable field must change — `field is immutable` / `updates to
///     statefulset spec ... are forbidden`.
///
/// The operator's DESIRED object is always structurally valid (codegen emits
/// exactly one source per volume and one handler per probe), so this error
/// class is a property of the LIVE object, not the desired one — deleting and
/// recreating from the desired spec is the deterministic, non-flapping cure.
///
/// Guards (so it never flaps and never destroys a healthy object for a
/// desired-side bug):
///   * only recreate an object that already exists (an update). A first CREATE
///     that is Invalid is a genuine spec bug — surface it.
///   * a recreate whose CREATE is itself rejected returns the error (no loop);
///     the next reconcile re-applies and, finding the object gone, re-creates.
///   * a successful recreate leaves the child matching desired, so the next
///     apply is a clean no-op.
///
/// Intended for stateless / standalone-PVC children (Deployments). NOT used for
/// StatefulSets, whose delete would strand `volumeClaimTemplate` PVCs (data
/// loss); those adopt via a matching `storage.volumeName` instead.
pub async fn apply_or_recreate<K>(api: &Api<K>, obj: &K) -> Result<K>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + Debug,
{
    let name =
        obj.meta().name.clone().ok_or_else(|| {
            OperatorError::Config("apply_or_recreate: missing metadata.name".into())
        })?;
    let pp = PatchParams::apply(FIELD_MANAGER).force();
    match api.patch(&name, &pp, &Patch::Apply(obj)).await {
        Ok(out) => Ok(out),
        Err(e) => {
            let err = OperatorError::from(e);
            if !is_structural_conflict(&err) {
                return Err(err);
            }
            // Only recreate an existing object (the update path). A first-create
            // Invalid is a real spec bug and must surface, not delete anything.
            if api.get_opt(&name).await?.is_none() {
                return Err(err);
            }
            tracing::warn!(
                object = %name,
                error = %err,
                "apply rejected as Invalid (structural conflict on live object); deleting and recreating from desired spec"
            );
            // Background delete retains any standalone PVC the pod mounts.
            api.delete(&name, &DeleteParams::default()).await?;
            wait_until_gone(api, &name).await?;
            let out = api.patch(&name, &pp, &Patch::Apply(obj)).await?;
            Ok(out)
        }
    }
}

/// True for the 422/`Invalid` responses that a fresh recreate cures: a stale
/// duplicate source/handler SSA cannot remove, or an immutable-field change.
/// Pure over the error so it is unit-testable without a cluster.
pub(crate) fn is_structural_conflict(err: &OperatorError) -> bool {
    let OperatorError::KubeApi(kube::Error::Api(resp)) = err else {
        return false;
    };
    if resp.code != 422 {
        return false;
    }
    let m = resp.message.as_str();
    m.contains("may not specify more than 1")
        || m.contains("field is immutable")
        || m.contains("updates to statefulset spec")
}

/// Poll until the named object is gone (deletion finalized), bounded to ~30s.
/// Falls through on timeout — the subsequent apply surfaces any real problem.
async fn wait_until_gone<K>(api: &Api<K>, name: &str) -> Result<()>
where
    K: Resource<DynamicType = ()> + Serialize + DeserializeOwned + Clone + Debug,
{
    for _ in 0..60 {
        if api.get_opt(name).await?.is_none() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Ok(())
}

/// Apply a DynamicObject (used for CRDs whose types are not statically known,
/// like KMSSecret). `api` carries the ApiResource on its DynamicType. Uses the
/// canonical `hanzo-operator` field manager.
pub async fn apply_dynamic(api: &Api<DynamicObject>, obj: &DynamicObject) -> Result<DynamicObject> {
    apply_dynamic_as(api, obj, FIELD_MANAGER).await
}

/// Apply a DynamicObject under an explicit SSA field manager. A reconcile source
/// distinct from the CR→child controllers (e.g. the native git→CR loop) applies
/// under its OWN manager so its edits are attributable and never silently fight
/// another manager's owned fields. Same force-conflicts semantics as
/// [`apply_dynamic`]. The GitSource controller uses the `gitops` manager here,
/// matching the field manager the retired reconcile cron used.
pub async fn apply_dynamic_as(
    api: &Api<DynamicObject>,
    obj: &DynamicObject,
    field_manager: &str,
) -> Result<DynamicObject> {
    let name = obj.metadata.name.clone().ok_or_else(|| {
        crate::core::OperatorError::Config("apply_dynamic: missing metadata.name".into())
    })?;
    let pp = PatchParams::apply(field_manager).force();
    let out = api.patch(&name, &pp, &Patch::Apply(obj)).await?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::Status;

    fn api_err(code: u16, message: &str) -> OperatorError {
        OperatorError::KubeApi(kube::Error::Api(Box::new(Status {
            code,
            message: message.into(),
            reason: "Invalid".into(),
            ..Default::default()
        })))
    }

    // A stale duplicate volume source (server-defaulted emptyDir beside our PVC)
    // is a live-side conflict a fresh recreate cures → recreate.
    #[test]
    fn duplicate_volume_type_is_structural_conflict() {
        assert!(is_structural_conflict(&api_err(
            422,
            "Deployment.apps \"search-fts5\" is invalid: spec.template.spec.volumes[0].persistentVolumeClaim: Forbidden: may not specify more than 1 volume type"
        )));
    }

    // An immutable StatefulSet spec change is recreate-fixable too.
    #[test]
    fn immutable_statefulset_is_structural_conflict() {
        assert!(is_structural_conflict(&api_err(
            422,
            "StatefulSet.apps \"sql\" is invalid: spec: Forbidden: updates to statefulset spec for fields other than 'replicas' ... are forbidden"
        )));
    }

    // A desired-side bug with NO duplicate/immutable signal (e.g. a bare missing
    // volume) must NOT trigger a destructive delete — surface the error instead.
    #[test]
    fn plain_not_found_is_not_a_structural_conflict() {
        assert!(!is_structural_conflict(&api_err(
            422,
            "Deployment.apps \"x\" is invalid: spec.template.spec.containers[0].volumeMounts[0].name: Not found: \"data\""
        )));
    }

    // Only 422/Invalid qualifies — a 409 conflict or 404 is retried/surfaced,
    // never recreated.
    #[test]
    fn non_422_is_not_a_structural_conflict() {
        assert!(!is_structural_conflict(&api_err(
            409,
            "Operation cannot be fulfilled: the object has been modified"
        )));
        assert!(!is_structural_conflict(&OperatorError::Config("x".into())));
    }

    // ---- Service adoption: clusterIP is a preserved value, never re-minted ----

    use k8s_openapi::api::core::v1::ServiceSpec;

    fn svc_with_cluster_ip(cluster_ip: Option<&str>, ips: Option<Vec<&str>>) -> Service {
        Service {
            spec: Some(ServiceSpec {
                type_: Some("ClusterIP".into()),
                cluster_ip: cluster_ip.map(str::to_string),
                cluster_ips: ips.map(|v| v.into_iter().map(str::to_string).collect()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // An apiserver-assigned clusterIP in the desired object is stripped, so the
    // SSA patch never carries the immutable field — the apiserver keeps the live
    // value and adoption is byte-stable: no fresh IP, no endpoint gap.
    #[test]
    fn scrub_strips_an_assigned_clusterip_for_in_place_adoption() {
        let mut svc = svc_with_cluster_ip(Some("10.124.38.86"), Some(vec!["10.124.38.86"]));
        scrub_assigned_cluster_ip(&mut svc);
        let spec = svc.spec.as_ref().unwrap();
        assert!(
            spec.cluster_ip.is_none(),
            "an assigned clusterIP must never reach the SSA patch"
        );
        assert!(
            spec.cluster_ips.is_none(),
            "clusterIPs is scrubbed in lockstep"
        );
        // The operative guarantee: the sent JSON has no clusterIP key at all.
        let json = serde_json::to_string(&svc).unwrap();
        assert!(
            !json.contains("clusterIP"),
            "scrubbed Service must not serialize clusterIP: {json}"
        );
    }

    // A headless Service's identity IS clusterIP:None — a declared shape that must
    // be sent, so the scrub leaves it untouched.
    #[test]
    fn scrub_preserves_the_headless_none_sentinel() {
        let mut svc = svc_with_cluster_ip(Some("None"), None);
        scrub_assigned_cluster_ip(&mut svc);
        assert_eq!(
            svc.spec.unwrap().cluster_ip.as_deref(),
            Some("None"),
            "headless identity is declared, not assigned — it must survive the scrub"
        );
    }

    // The steady state: a desired Service that already omits clusterIP (what
    // `manifests::build_service` emits) is unchanged — the scrub is a no-op, so
    // routing every Service through `apply_service` never alters today's behavior.
    #[test]
    fn scrub_leaves_an_already_absent_clusterip_absent() {
        let mut svc = svc_with_cluster_ip(None, None);
        scrub_assigned_cluster_ip(&mut svc);
        assert!(svc.spec.unwrap().cluster_ip.is_none());
    }

    // Fail-safe over a spec-less Service: no panic, nothing to scrub.
    #[test]
    fn scrub_on_a_specless_service_is_a_noop() {
        let mut svc = Service::default();
        scrub_assigned_cluster_ip(&mut svc);
        assert!(svc.spec.is_none());
    }
}
