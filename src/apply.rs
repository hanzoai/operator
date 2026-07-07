//! Idempotent server-side apply for managed K8s objects.
//!
//! Wraps `Api::patch` with `PatchParams::apply("hanzo-operator")` so each
//! reconcile is an SSA round — the operator owns its declared fields, and
//! anything edited out-of-band reverts on next loop.

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, Patch, PatchParams};
use kube::core::DynamicObject;
use kube::Resource;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Debug;

use crate::core::Result;

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

/// Server-side apply a ConfigMap, refusing to clobber a populated ConfigMap
/// with empty data.
///
/// A ConfigMap whose `data` AND `binaryData` are both empty carries no config.
/// Force-applying it strips every key the `hanzo-operator` field manager owns,
/// blanking the workload's mounted config file → CrashLoopBackOff (the hanzo.id
/// 30-min auth outage: an empty CR config source regenerated `iam-conf` empty →
/// `panic: unable to open database file`; likewise `otel-collector-config`). The
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
