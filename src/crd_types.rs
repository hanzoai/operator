//! CRD-friendly type wrappers.
//!
//! k8s-openapi types (`EnvVar`, `Volume`, `Condition`, etc.) don't implement
//! `schemars::JsonSchema`, so we can't embed them directly in CRD specs.
//! We mirror the wire shape in our own types and convert to/from k8s-openapi
//! at the boundary inside controllers.
//!
//! Wire shape is byte-identical with k8s-openapi — same JSON keys, same
//! defaults. CRs in the cluster don't notice the swap.

use k8s_openapi::api::core::v1::{
    ConfigMapVolumeSource as K8sConfigMapVolumeSource, EmptyDirVolumeSource as K8sEmptyDirVolumeSource,
    EnvFromSource as K8sEnvFromSource, EnvVar as K8sEnvVar, EnvVarSource as K8sEnvVarSource,
    KeyToPath as K8sKeyToPath, LocalObjectReference as K8sLocalObjectReference,
    PersistentVolumeClaimVolumeSource as K8sPersistentVolumeClaimVolumeSource,
    SecretReference as K8sSecretReference, SecretVolumeSource as K8sSecretVolumeSource,
    Volume as K8sVolume, VolumeMount as K8sVolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition as K8sCondition, Time as K8sTime};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnvVar {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_from: Option<EnvVarSource>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnvVarSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_key_ref: Option<ConfigMapKeySelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_ref: Option<SecretKeySelector>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_ref: Option<ObjectFieldSelector>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapKeySelector {
    pub name: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecretKeySelector {
    pub name: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ObjectFieldSelector {
    pub field_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_version: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EnvFromSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map_ref: Option<ConfigMapEnvSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<SecretEnvSource>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapEnvSource {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecretEnvSource {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct VolumeMount {
    pub name: String,
    pub mount_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sub_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// A pod volume: a name plus exactly one typed source. Each source is a real
/// object schema (not a freeform `serde_json::Value`), so it survives into the
/// generated structural CRD — a `serde_json::Value` source renders as an
/// untyped blob that the apiserver prunes, which silently dropped `configMap` /
/// `persistentVolumeClaim` from `spec.volumes[*]` and broke config/PVC mounts.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Volume {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_map: Option<ConfigMapVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<SecretVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistent_volume_claim: Option<PersistentVolumeClaimVolumeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_dir: Option<EmptyDirVolumeSource>,
}

/// One key→path projection inside a `configMap` / `secret` volume source.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct KeyToPath {
    pub key: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<i32>,
}

/// `configMap` volume source — mounts a ConfigMap by name.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConfigMapVolumeSource {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<KeyToPath>,
}

/// `secret` volume source — mounts a Secret by name.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecretVolumeSource {
    pub secret_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_mode: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub optional: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<KeyToPath>,
}

/// `persistentVolumeClaim` volume source — mounts an existing PVC.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PersistentVolumeClaimVolumeSource {
    pub claim_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// `emptyDir` volume source — an ephemeral scratch volume.
#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EmptyDirVolumeSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_limit: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct LocalObjectReference {
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct SecretReference {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Container {
    pub name: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env: Vec<EnvVar>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<EnvFromSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volume_mounts: Vec<VolumeMount>,
    /// Image pull policy (Always / IfNotPresent / Never).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image_pull_policy: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_transition_time: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema, Default)]
pub struct Time(pub String);

// ---- Conversion to k8s-openapi types (used inside controllers) ----

impl EnvVar {
    pub fn to_k8s(&self) -> K8sEnvVar {
        K8sEnvVar {
            name: self.name.clone(),
            value: self.value.clone(),
            value_from: self.value_from.as_ref().map(|s| K8sEnvVarSource {
                config_map_key_ref: s.config_map_key_ref.as_ref().map(|c| {
                    k8s_openapi::api::core::v1::ConfigMapKeySelector {
                        name: c.name.clone(),
                        key: c.key.clone(),
                        optional: c.optional,
                    }
                }),
                secret_key_ref: s.secret_key_ref.as_ref().map(|c| {
                    k8s_openapi::api::core::v1::SecretKeySelector {
                        name: c.name.clone(),
                        key: c.key.clone(),
                        optional: c.optional,
                    }
                }),
                field_ref: s.field_ref.as_ref().map(|c| {
                    k8s_openapi::api::core::v1::ObjectFieldSelector {
                        api_version: if c.api_version.is_empty() {
                            None
                        } else {
                            Some(c.api_version.clone())
                        },
                        field_path: c.field_path.clone(),
                    }
                }),
                ..Default::default()
            }),
        }
    }
}

impl EnvFromSource {
    pub fn to_k8s(&self) -> K8sEnvFromSource {
        K8sEnvFromSource {
            config_map_ref: self.config_map_ref.as_ref().map(|c| {
                k8s_openapi::api::core::v1::ConfigMapEnvSource {
                    name: c.name.clone(),
                    optional: c.optional,
                }
            }),
            secret_ref: self.secret_ref.as_ref().map(|c| {
                k8s_openapi::api::core::v1::SecretEnvSource {
                    name: c.name.clone(),
                    optional: c.optional,
                }
            }),
            prefix: if self.prefix.is_empty() {
                None
            } else {
                Some(self.prefix.clone())
            },
        }
    }
}

impl VolumeMount {
    pub fn to_k8s(&self) -> K8sVolumeMount {
        K8sVolumeMount {
            name: self.name.clone(),
            mount_path: self.mount_path.clone(),
            sub_path: if self.sub_path.is_empty() {
                None
            } else {
                Some(self.sub_path.clone())
            },
            read_only: self.read_only,
            ..Default::default()
        }
    }
}

/// Convert our key→path items into the k8s form, `None` when empty (k8s omits
/// an absent projection).
fn to_key_paths(items: &[KeyToPath]) -> Option<Vec<K8sKeyToPath>> {
    if items.is_empty() {
        return None;
    }
    Some(
        items
            .iter()
            .map(|i| K8sKeyToPath {
                key: i.key.clone(),
                path: i.path.clone(),
                mode: i.mode,
            })
            .collect(),
    )
}

impl Volume {
    pub fn to_k8s(&self) -> K8sVolume {
        K8sVolume {
            name: self.name.clone(),
            config_map: self.config_map.as_ref().map(|c| K8sConfigMapVolumeSource {
                name: c.name.clone(),
                default_mode: c.default_mode,
                optional: c.optional,
                items: to_key_paths(&c.items),
            }),
            secret: self.secret.as_ref().map(|s| K8sSecretVolumeSource {
                secret_name: Some(s.secret_name.clone()),
                default_mode: s.default_mode,
                optional: s.optional,
                items: to_key_paths(&s.items),
            }),
            persistent_volume_claim: self.persistent_volume_claim.as_ref().map(|p| {
                K8sPersistentVolumeClaimVolumeSource {
                    claim_name: p.claim_name.clone(),
                    read_only: p.read_only,
                }
            }),
            empty_dir: self.empty_dir.as_ref().map(|e| K8sEmptyDirVolumeSource {
                medium: e.medium.clone(),
                size_limit: e.size_limit.as_ref().map(|q| Quantity(q.clone())),
            }),
            ..Default::default()
        }
    }
}

impl LocalObjectReference {
    pub fn to_k8s(&self) -> K8sLocalObjectReference {
        K8sLocalObjectReference {
            name: self.name.clone(),
        }
    }
}

impl SecretReference {
    pub fn to_k8s(&self) -> K8sSecretReference {
        K8sSecretReference {
            name: Some(self.name.clone()),
            namespace: if self.namespace.is_empty() {
                None
            } else {
                Some(self.namespace.clone())
            },
        }
    }
}

impl Container {
    pub fn to_k8s(&self) -> k8s_openapi::api::core::v1::Container {
        k8s_openapi::api::core::v1::Container {
            name: self.name.clone(),
            image: Some(self.image.clone()),
            command: if self.command.is_empty() {
                None
            } else {
                Some(self.command.clone())
            },
            args: if self.args.is_empty() {
                None
            } else {
                Some(self.args.clone())
            },
            env: if self.env.is_empty() {
                None
            } else {
                Some(self.env.iter().map(EnvVar::to_k8s).collect())
            },
            env_from: if self.env_from.is_empty() {
                None
            } else {
                Some(self.env_from.iter().map(EnvFromSource::to_k8s).collect())
            },
            volume_mounts: if self.volume_mounts.is_empty() {
                None
            } else {
                Some(self.volume_mounts.iter().map(VolumeMount::to_k8s).collect())
            },
            image_pull_policy: if self.image_pull_policy.is_empty() {
                None
            } else {
                Some(self.image_pull_policy.clone())
            },
            ..Default::default()
        }
    }
}

impl Condition {
    pub fn to_k8s(&self) -> K8sCondition {
        K8sCondition {
            type_: self.type_.clone(),
            status: self.status.clone(),
            reason: self.reason.clone(),
            message: self.message.clone(),
            // k8s-openapi 0.28 backs meta/v1 Time with jiff::Timestamp (not
            // chrono). Our wire wrapper stays an RFC3339 string; parse it into a
            // jiff timestamp, falling back to now on a malformed/absent value.
            last_transition_time: self
                .last_transition_time
                .as_ref()
                .and_then(|t| t.0.parse::<jiff::Timestamp>().ok())
                .map(K8sTime)
                .unwrap_or_else(|| K8sTime(jiff::Timestamp::now())),
            observed_generation: self.observed_generation,
        }
    }

    pub fn from_k8s(c: &K8sCondition) -> Self {
        Condition {
            type_: c.type_.clone(),
            status: c.status.clone(),
            reason: c.reason.clone(),
            message: c.message.clone(),
            // jiff::Timestamp's Display is RFC3339 — the same shape our wrapper
            // carries, so the round-trip through the CRD wire stays valid.
            last_transition_time: Some(Time(c.last_transition_time.0.to_string())),
            observed_generation: c.observed_generation,
        }
    }
}

/// Build a Condition (CRD-friendly) — same wire shape as k8s.
pub fn build_condition(
    type_: &str,
    status: bool,
    reason: &str,
    message: &str,
    generation: i64,
) -> Condition {
    Condition {
        type_: type_.to_string(),
        status: if status {
            "True".to_string()
        } else {
            "False".to_string()
        },
        reason: reason.to_string(),
        message: message.to_string(),
        // jiff::Timestamp's Display is RFC3339 — the wire shape our `Time(String)`
        // wrapper carries (the operator's only timestamp library, matching the
        // jiff-backed k8s meta/v1 Time at the boundary).
        last_transition_time: Some(Time(jiff::Timestamp::now().to_string())),
        observed_generation: Some(generation),
    }
}

/// Preserve `last_transition_time` from a prior condition of the same `type_`
/// when its `status` (`True`/`False`) has NOT flipped. Per the Kubernetes
/// condition convention, `lastTransitionTime` advances ONLY on a genuine state
/// transition. `build_condition` stamps `now()` unconditionally, so without
/// this a controller re-stamps the timestamp every reconcile — the CR's status
/// then changes on every pass, the controller's own watch re-delivers it as an
/// `object updated` event, and the reconcile self-triggers into a hot loop.
pub fn carry_transition_time(prior: &[Condition], next: &mut Condition) {
    if let Some(prev) = prior.iter().find(|c| c.type_ == next.type_) {
        if prev.status == next.status {
            next.last_transition_time = prev.last_transition_time.clone();
        }
    }
}

/// True when two serializable status values differ structurally. Controllers
/// use this to SKIP a no-op `patch_status`: a `force()` merge bumps
/// `resourceVersion` every reconcile even when nothing changed, and the watch
/// re-delivers that as `object updated`, self-triggering the next reconcile.
/// Skipping identical writes breaks that loop at the source.
pub fn status_changed<T: Serialize>(new: &T, old: &T) -> bool {
    serde_json::to_value(new).ok() != serde_json::to_value(old).ok()
}

/// Replace the condition of the same `type_` in place, or append it. The one
/// upsert used by every controller that writes CRD-wrapper conditions (Service,
/// GitSource, ImageUpdate) — the k8s-openapi twin lives in `core::status` for
/// controllers on that condition type.
pub fn upsert_condition(conditions: &mut Vec<Condition>, new_cond: Condition) {
    if let Some(slot) = conditions.iter_mut().find(|c| c.type_ == new_cond.type_) {
        *slot = new_cond;
    } else {
        conditions.push(new_cond);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `configMap` volume source (e.g. `status`'s gatus config) must survive
    /// the CR-JSON → `Volume` deserialize → `to_k8s` round-trip. The typed
    /// source is the load-bearing part: if it drops the source, the operator
    /// emits a sourceless volume and the app's config never mounts (the
    /// `status` gatus "configuration file not found" crash).
    #[test]
    fn volume_configmap_source_survives_round_trip() {
        let cr = serde_json::json!({"name": "config", "configMap": {"name": "status-config"}});
        let v: Volume = serde_json::from_value(cr).expect("deserialize volume");
        assert_eq!(v.name, "config");
        assert!(
            v.config_map.is_some(),
            "typed source must capture the configMap key",
        );
        let k = v.to_k8s();
        assert_eq!(k.name, "config");
        let cm = k.config_map.expect("config_map source must survive to_k8s");
        assert_eq!(cm.name, "status-config");
    }

    /// A `persistentVolumeClaim` source (e.g. `status`'s data dir) must survive
    /// the same round-trip.
    #[test]
    fn volume_pvc_source_survives_round_trip() {
        let cr = serde_json::json!({
            "name": "data",
            "persistentVolumeClaim": {"claimName": "status-data"}
        });
        let v: Volume = serde_json::from_value(cr).expect("deserialize volume");
        let k = v.to_k8s();
        let pvc = k
            .persistent_volume_claim
            .expect("pvc source must survive to_k8s");
        assert_eq!(pvc.claim_name, "status-data");
    }

    /// An env var sourced from a secret (with `optional: true`) must produce a
    /// `valueFrom` and NO `value` — never both. k8s rejects an EnvVar carrying
    /// both (`may not be specified when value is not empty`), which is the
    /// `sign` reconcile failure mode.
    #[test]
    fn env_var_secret_ref_has_value_from_not_value() {
        let e = EnvVar {
            name: "NEXTAUTH_SECRET".into(),
            value: None,
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: "sign-secrets".into(),
                    key: "NEXTAUTH_SECRET".into(),
                    optional: Some(true),
                }),
                ..Default::default()
            }),
        };
        let k = e.to_k8s();
        assert!(k.value.is_none(), "secret-sourced env must not set value");
        let vf = k.value_from.expect("value_from must be set");
        assert_eq!(
            vf.secret_key_ref.expect("secret_key_ref").optional,
            Some(true)
        );
    }
}
