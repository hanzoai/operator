//! Controllers for each CRD Kind.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Resource;

pub mod baseapp;
pub mod compat;
pub mod datastore;
pub mod dns;
pub mod gateway;
pub mod ingress;
pub mod mpc;
pub mod network;
pub mod service;

// Re-export shared inner functions for compat facades.
pub use datastore::reconcile_datastore_inner_pub as datastore_inner_for_compat;
pub use service::reconcile_service_inner_pub as service_inner_for_compat;

/// Build an OwnerReference pointing at a CR. The CR must have a UID set.
pub fn owner_ref_for<K>(cr: &K, api_version: &str, kind: &str) -> OwnerReference
where
    K: Resource<DynamicType = ()>,
{
    OwnerReference {
        api_version: api_version.to_string(),
        kind: kind.to_string(),
        name: cr.meta().name.clone().unwrap_or_default(),
        uid: cr.meta().uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}
