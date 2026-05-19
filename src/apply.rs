//! Idempotent server-side apply for managed K8s objects.
//!
//! Wraps `Api::patch` with `PatchParams::apply("hanzo-operator")` so each
//! reconcile is an SSA round — the operator owns its declared fields, and
//! anything edited out-of-band reverts on next loop.

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
