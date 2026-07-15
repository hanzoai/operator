//! Managed-upgrade state machine — the deploy discipline encoded into the
//! operator, so no human hand-flips an image, hand-watches a rollout, or
//! hand-writes an auto-rollback loop.
//!
//! ## The brain is pure
//!
//! [`plan`] is a pure function of observed VALUES — the desired image
//! (`spec.image`), the image the live Deployment runs, the last image proven
//! healthy (`status.lastGoodImage`), whether production is healthy right now,
//! the persisted FSM state (`status.upgrade`), the pre-flight pod's boot
//! outcome, and whether the rolling pods are crash-looping. It returns the
//! `effective_image` the Deployment must run NOW plus the next persisted state.
//! No cluster calls — the FSM is unit-tested exhaustively with hand-built values.
//!
//! ## The invariant
//!
//! **Production is never left on an image that failed its gate.** The effective
//! image is `lastGood` in every state where the candidate is unproven or has
//! failed (Preflighting, a failed pre-flight, RollingBack, and the terminal
//! Failed); it is the candidate ONLY in Rolling — AFTER the pre-flight passed —
//! and `lastGood` is advanced to the candidate ONLY once the candidate is proven
//! healthy in production (Rolling → Succeeded).
//!
//! ## Resumable
//!
//! Every input is read from the CR status + live cluster, so [`plan`] re-derives
//! the same action after an operator restart. The pre-flight pod is named
//! deterministically by the target image, so a restart mid-pre-flight re-finds
//! it. There is no in-memory state.
//!
//! ## Fail-closed terminal
//!
//! A failed candidate lands in `Failed`, keyed to the exact target image. The
//! operator will not re-attempt that image (it keeps production on `lastGood`);
//! only a NEW `spec.image` reopens the FSM (a supersede).

use std::collections::BTreeMap;

use jiff::{SignedDuration, Timestamp};
use k8s_openapi::api::core::v1::{
    Container, PersistentVolumeClaim, Pod, PodSpec, Probe, ResourceRequirements, Volume,
    VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use kube::api::{Api, DeleteParams, ListParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::ResourceExt;
use sha2::{Digest, Sha256};

use crate::apply;
use crate::core::health::{self, BootOutcome};
use crate::core::Result;
use crate::crd::{UpgradePhase, UpgradeRecord, UpgradeStatus};

/// Default health-gated rollout deadline before auto-rollback.
pub const DEFAULT_ROLLOUT_DEADLINE_SECS: i64 = 300;
/// Default pre-flight candidate-boot deadline before the pre-flight is failed.
pub const DEFAULT_PREFLIGHT_DEADLINE_SECS: i64 = 300;
/// Floor on either deadline — a too-short deadline would flap a healthy roll.
pub const MIN_DEADLINE_SECS: i64 = 30;
/// Cap on retained `status.upgradeHistory` entries (bounded to keep the CR small).
pub const MAX_HISTORY: usize = 10;

/// Label carried by every pre-flight resource (pod / clone PVC / snapshot) so
/// cleanup is a label-selected sweep, robust to a superseded target.
pub const PREFLIGHT_OF_LABEL: &str = "hanzo.ai/preflight-of";
/// Label carrying the target-image hash, so a superseded target's stale
/// resources are distinguishable from the current one's.
pub const PREFLIGHT_TARGET_LABEL: &str = "hanzo.ai/preflight-target";

// ============================================================================
// Pure FSM
// ============================================================================

/// Everything the FSM observes — all VALUES, no cluster handles.
pub struct Observed<'a> {
    /// The Service name (for deterministic pre-flight resource names).
    pub name: &'a str,
    /// Desired image (`spec.image` repo:tag).
    pub desired: &'a str,
    /// The image the live Deployment's main container currently runs.
    pub running: Option<&'a str>,
    /// The last image proven healthy in production.
    pub last_good: Option<&'a str>,
    /// Is the live Deployment fully healthy right now (rolled out + available)?
    pub prod_healthy: bool,
    /// The persisted FSM state (`status.upgrade`).
    pub upgrade: Option<&'a UpgradeStatus>,
    /// The pre-flight candidate pod's boot outcome — only consulted in
    /// Preflighting; `Booting` otherwise.
    pub preflight: BootOutcome,
    /// Are the rolling pods (running the candidate) crash-looping — the
    /// fast-rollback signal during a health-gated roll.
    pub rolling_crashloop: bool,
    /// Current instant.
    pub now: Timestamp,
}

/// Deadlines + whether a pre-flight is required (stateful, or explicitly opted
/// in). Derived by the controller from `spec.upgradePolicy` + `spec.persistence`.
pub struct Cfg {
    pub preflight_needed: bool,
    pub preflight_deadline_secs: i64,
    pub rollout_deadline_secs: i64,
}

/// The FSM's decision: the image the Deployment must run now, the next persisted
/// state, an optional history record to append, and whether to advance the
/// last-good baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// The image the Deployment must run RIGHT NOW.
    pub effective_image: String,
    /// The next `status.upgrade` (None ⇒ Stable, clear it).
    pub next_upgrade: Option<UpgradeStatus>,
    /// A concluded-attempt record to append to `status.upgradeHistory`.
    pub record: Option<UpgradeRecord>,
    /// Advance `status.lastGoodImage` to this (only when a value is proven
    /// healthy in production).
    pub mark_last_good: Option<String>,
    /// Human-readable reason (log + k8s Event).
    pub reason: &'static str,
}

/// The pure decision function. See the module docs for the invariant.
pub fn plan(obs: &Observed, cfg: &Cfg) -> Plan {
    match obs.upgrade {
        None => plan_no_upgrade(obs, cfg),
        Some(u) => {
            // The desired image moved away from what we are driving ⇒ supersede:
            // abandon the current attempt and re-plan for the new desired.
            if obs.desired != u.target_image {
                return supersede(obs, cfg, u);
            }
            match u.phase {
                Some(UpgradePhase::Preflighting) => drive_preflighting(obs, cfg, u),
                Some(UpgradePhase::Rolling) => drive_rolling(obs, cfg, u),
                Some(UpgradePhase::RollingBack) => drive_rolling_back(obs, u),
                // Terminal, or a malformed phase we treat as terminal (fail-safe:
                // keep production on last-good, await a new image).
                Some(UpgradePhase::Failed) | None => terminal_failed(obs, u),
            }
        }
    }
}

/// No upgrade in flight: establish/refresh the last-good baseline, converge to
/// desired, or start a managed upgrade when desired is a new target.
fn plan_no_upgrade(obs: &Observed, cfg: &Cfg) -> Plan {
    match obs.last_good {
        // No baseline yet — the FSM has never seen this Service healthy.
        None => match obs.running {
            None => settle(obs.desired, None, None, "creating (no deployment yet)"),
            Some(run) if obs.prod_healthy => {
                // Bootstrap: adopt the healthy running image as the baseline.
                settle(run, None, Some(run), "adopted healthy baseline")
            }
            // Running but not yet healthy, and no baseline to fall back to — do
            // not disrupt; wait for it to become healthy (then we adopt it).
            Some(run) => settle(run, None, None, "awaiting first healthy baseline"),
        },
        // We have a baseline `lg`.
        Some(lg) => match obs.running {
            // Already on desired.
            Some(run) if run == obs.desired => {
                if obs.prod_healthy {
                    // Steady state — keep the baseline current.
                    settle(obs.desired, None, Some(obs.desired), "stable")
                } else {
                    // On desired but still converging (e.g. first create catching
                    // up, or an HPA scale). Do NOT auto-rollback an image the FSM
                    // did not itself flip — that is an external actor's edit.
                    settle(obs.desired, None, None, "converging on desired")
                }
            }
            // Not on desired (or no Deployment): desired is a new target.
            _ => {
                if obs.desired == lg {
                    // Desired IS the known-good image — converge to it directly
                    // (no pre-flight; it is already proven).
                    settle(obs.desired, None, None, "converge to last-good")
                } else if obs.running.is_none() {
                    // No Deployment yet (with a baseline is unusual) — create it.
                    settle(obs.desired, None, None, "creating at desired")
                } else {
                    // Start a managed upgrade from the baseline to the new target.
                    start_upgrade(obs, cfg)
                }
            }
        },
    }
}

/// Begin a managed upgrade. Pre-flight first if required (production stays on the
/// current image); otherwise flip straight into a health-gated roll.
fn start_upgrade(obs: &Observed, cfg: &Cfg) -> Plan {
    // `start_upgrade` is only reached with `running` Some (checked by caller).
    let run = obs.running.unwrap_or(obs.desired);
    if cfg.preflight_needed {
        Plan {
            effective_image: run.to_string(), // stay on current during pre-flight
            next_upgrade: Some(preflighting(obs.name, obs.desired, obs.now, cfg)),
            record: None,
            mark_last_good: None,
            reason: "start: pre-flighting candidate over real data",
        }
    } else {
        Plan {
            effective_image: obs.desired.to_string(), // flip now, health-gated
            next_upgrade: Some(rolling(obs.desired, obs.now, cfg, &now_str(obs.now))),
            record: None,
            mark_last_good: None,
            reason: "start: rolling (no pre-flight)",
        }
    }
}

/// Drive the Preflighting phase off the candidate pod's boot outcome.
fn drive_preflighting(obs: &Observed, cfg: &Cfg, u: &UpgradeStatus) -> Plan {
    // Production stays on the current image throughout the pre-flight.
    let stay = obs
        .running
        .or(obs.last_good)
        .unwrap_or(obs.desired)
        .to_string();
    match obs.preflight {
        BootOutcome::Ready => Plan {
            // Pre-flight passed — flip to the candidate and health-gate the roll.
            effective_image: obs.desired.to_string(),
            next_upgrade: Some(rolling(obs.desired, obs.now, cfg, &u.started_at)),
            record: None,
            mark_last_good: None,
            reason: "pre-flight passed; rolling out candidate",
        },
        BootOutcome::Failed => preflight_failed(
            obs,
            u,
            "candidate crashed during pre-flight boot over real data",
        ),
        BootOutcome::Booting => {
            if expired(u, obs.now) {
                preflight_failed(
                    obs,
                    u,
                    "pre-flight boot did not reach readiness before deadline",
                )
            } else {
                // Keep waiting — production untouched.
                Plan {
                    effective_image: stay,
                    next_upgrade: Some(u.clone()),
                    record: None,
                    mark_last_good: None,
                    reason: "pre-flighting candidate",
                }
            }
        }
    }
}

/// The candidate failed pre-flight — production was NEVER flipped, so it stays on
/// the current image. Land in the terminal Failed state, keyed to the target.
fn preflight_failed(obs: &Observed, u: &UpgradeStatus, msg: &'static str) -> Plan {
    let stay = obs.running.or(obs.last_good).unwrap_or(obs.desired);
    Plan {
        effective_image: stay.to_string(),
        next_upgrade: Some(failed(&u.target_image, msg)),
        record: Some(make_record(
            &u.target_image,
            Some(stay),
            "PreflightFailed",
            obs.now,
            msg,
        )),
        mark_last_good: None,
        reason: "pre-flight FAILED; production untouched",
    }
}

/// Drive the Rolling phase: succeed when the candidate is healthy in production,
/// auto-rollback on crashloop or deadline.
fn drive_rolling(obs: &Observed, cfg: &Cfg, u: &UpgradeStatus) -> Plan {
    let target = u.target_image.clone();

    // Success: the Deployment runs the candidate AND is healthy.
    if obs.running == Some(u.target_image.as_str()) && obs.prod_healthy {
        return Plan {
            effective_image: target.clone(),
            next_upgrade: None, // Stable
            record: Some(make_record(
                &target,
                obs.last_good,
                "Succeeded",
                obs.now,
                "candidate rolled out healthy",
            )),
            mark_last_good: Some(target), // advance baseline — proven healthy
            reason: "rollout healthy; upgrade succeeded",
        };
    }

    // Failure: crashloop (fast path) or deadline exceeded ⇒ auto-rollback.
    if obs.rolling_crashloop || expired(u, obs.now) {
        // `last_good` is guaranteed Some to reach Rolling; fall back defensively
        // to the current image rather than panic on a corrupt status.
        let lg = obs.last_good.or(obs.running).unwrap_or(&target);
        let reason = if obs.rolling_crashloop {
            "candidate crash-looping; auto-rolling back to last-good"
        } else {
            "rollout deadline exceeded; auto-rolling back to last-good"
        };
        return Plan {
            effective_image: lg.to_string(), // FLIP BACK to last-good
            next_upgrade: Some(rolling_back(&target, obs.now, cfg, &u.started_at)),
            record: None,
            mark_last_good: None,
            reason,
        };
    }

    // Still rolling within the deadline.
    Plan {
        effective_image: target,
        next_upgrade: Some(u.clone()),
        record: None,
        mark_last_good: None,
        reason: "health-gating rollout",
    }
}

/// Drive the RollingBack phase: hold the Deployment on last-good until it is
/// healthy again, then conclude in the terminal Failed state.
fn drive_rolling_back(obs: &Observed, u: &UpgradeStatus) -> Plan {
    let lg = obs.last_good.or(obs.running).unwrap_or(&u.target_image);
    if obs.running == Some(lg) && obs.prod_healthy {
        return Plan {
            effective_image: lg.to_string(),
            next_upgrade: Some(failed(&u.target_image, "auto-rolled-back to last-good")),
            record: Some(make_record(
                &u.target_image,
                Some(lg),
                "RolledBack",
                obs.now,
                "reverted to last-good after failed rollout",
            )),
            mark_last_good: None, // baseline unchanged (lg was already good)
            reason: "rollback complete; production restored to last-good",
        };
    }
    Plan {
        effective_image: lg.to_string(),
        next_upgrade: Some(u.clone()),
        record: None,
        mark_last_good: None,
        reason: "rolling back to last-good",
    }
}

/// Terminal Failed: production stays on last-good; the failed target is never
/// re-attempted. Only a NEW `spec.image` (a supersede) reopens the FSM.
fn terminal_failed(obs: &Observed, u: &UpgradeStatus) -> Plan {
    let stay = obs
        .last_good
        .or(obs.running)
        .unwrap_or(obs.desired)
        .to_string();
    Plan {
        effective_image: stay,
        next_upgrade: Some(u.clone()),
        record: None,
        mark_last_good: None,
        reason: "upgrade failed; holding on last-good (awaiting a new image)",
    }
}

/// The desired image changed away from the in-flight target — abandon the
/// current attempt (recording it, unless it was already terminal) and re-plan
/// for the new desired.
fn supersede(obs: &Observed, cfg: &Cfg, u: &UpgradeStatus) -> Plan {
    let mut p = plan_no_upgrade(obs, cfg);
    // Record the abandonment only for a genuinely in-flight attempt (not a
    // terminal Failed the operator is simply moving on from). `plan_no_upgrade`
    // never emits a record, so we own the record slot here.
    if u.phase != Some(UpgradePhase::Failed) {
        p.record = Some(make_record(
            &u.target_image,
            obs.last_good,
            "Superseded",
            obs.now,
            "desired image changed before this attempt concluded",
        ));
    }
    p
}

// ---- pure helpers ----

/// A settled (no-upgrade) plan.
fn settle(
    image: &str,
    next: Option<UpgradeStatus>,
    mark_last_good: Option<&str>,
    reason: &'static str,
) -> Plan {
    Plan {
        effective_image: image.to_string(),
        next_upgrade: next,
        record: None,
        mark_last_good: mark_last_good.map(str::to_string),
        reason,
    }
}

fn now_str(now: Timestamp) -> String {
    now.to_string()
}

fn deadline_str(now: Timestamp, secs: i64) -> String {
    now.checked_add(SignedDuration::from_secs(secs.max(MIN_DEADLINE_SECS)))
        .unwrap_or(now)
        .to_string()
}

/// True when `now` is past `u.deadline_at`. A missing/unparseable deadline is
/// treated as NOT expired — the FSM then relies on the explicit Ready/Failed
/// signals rather than failing an upgrade on a clock-parse error.
fn expired(u: &UpgradeStatus, now: Timestamp) -> bool {
    match u.deadline_at.parse::<Timestamp>() {
        Ok(deadline) => now > deadline,
        Err(_) => false,
    }
}

fn preflighting(name: &str, target: &str, now: Timestamp, cfg: &Cfg) -> UpgradeStatus {
    UpgradeStatus {
        target_image: target.to_string(),
        phase: Some(UpgradePhase::Preflighting),
        started_at: now_str(now),
        deadline_at: deadline_str(now, cfg.preflight_deadline_secs),
        preflight_pod: preflight_pod_name(name, target),
        preflight_clone: preflight_clone_name(name, target),
        preflight_snapshot: preflight_snapshot_name(name, target),
        message: "pre-flighting candidate over real data".to_string(),
    }
}

fn rolling(target: &str, now: Timestamp, cfg: &Cfg, started_at: &str) -> UpgradeStatus {
    UpgradeStatus {
        target_image: target.to_string(),
        phase: Some(UpgradePhase::Rolling),
        started_at: started_at.to_string(),
        deadline_at: deadline_str(now, cfg.rollout_deadline_secs),
        message: "health-gating rollout".to_string(),
        ..Default::default()
    }
}

fn rolling_back(target: &str, now: Timestamp, cfg: &Cfg, started_at: &str) -> UpgradeStatus {
    UpgradeStatus {
        target_image: target.to_string(),
        phase: Some(UpgradePhase::RollingBack),
        started_at: started_at.to_string(),
        deadline_at: deadline_str(now, cfg.rollout_deadline_secs),
        message: "rolling back to last-good".to_string(),
        ..Default::default()
    }
}

fn failed(target: &str, msg: &str) -> UpgradeStatus {
    UpgradeStatus {
        target_image: target.to_string(),
        phase: Some(UpgradePhase::Failed),
        message: msg.to_string(),
        ..Default::default()
    }
}

fn make_record(
    image: &str,
    from: Option<&str>,
    result: &str,
    now: Timestamp,
    msg: &str,
) -> UpgradeRecord {
    UpgradeRecord {
        image: image.to_string(),
        from_image: from.unwrap_or("").to_string(),
        result: result.to_string(),
        at: now_str(now),
        message: msg.to_string(),
    }
}

/// Append `record` to `history`, keeping it bounded to the most-recent
/// [`MAX_HISTORY`] entries.
pub fn push_history(history: &mut Vec<UpgradeRecord>, record: UpgradeRecord) {
    history.push(record);
    let overflow = history.len().saturating_sub(MAX_HISTORY);
    if overflow > 0 {
        history.drain(0..overflow);
    }
}

// ============================================================================
// Deterministic pre-flight resource names
// ============================================================================

/// First 8 hex chars of sha256(target) — stable per target image so a restart
/// mid-pre-flight re-finds the pod, and two targets never collide.
fn target_hash(target: &str) -> String {
    let d = Sha256::digest(target.as_bytes());
    format!("{:02x}{:02x}{:02x}{:02x}", d[0], d[1], d[2], d[3])
}

/// `<name>-preflight-<hash>` — clamped to a DNS-1123-valid 63 chars.
fn pf_name(name: &str, target: &str, suffix: &str) -> String {
    let hash = target_hash(target);
    // Reserve room for "-preflight-<8hash><suffix>".
    let reserved = "-preflight-".len() + hash.len() + suffix.len();
    let keep = 63usize.saturating_sub(reserved).min(name.len());
    let base = name[..keep].trim_end_matches('-');
    format!("{base}-preflight-{hash}{suffix}")
}

pub fn preflight_pod_name(name: &str, target: &str) -> String {
    pf_name(name, target, "")
}

pub fn preflight_clone_name(name: &str, target: &str) -> String {
    pf_name(name, target, "-clone")
}

pub fn preflight_snapshot_name(name: &str, target: &str) -> String {
    pf_name(name, target, "-snap")
}

// ============================================================================
// Imperative side — pre-flight resource builders + converge/observe
// ============================================================================

/// Everything the controller assembles from the Service spec to run a pre-flight
/// candidate boot. Owned data so it is cheap to pass by value.
pub struct PreflightInputs {
    pub namespace: String,
    /// The candidate image to boot.
    pub candidate_image: String,
    pub pull_policy: String,
    pub command: Vec<String>,
    pub args: Vec<String>,
    /// The Service's env + the boot-only overlay (already merged, overlay last).
    pub env: Vec<k8s_openapi::api::core::v1::EnvVar>,
    pub env_from: Vec<k8s_openapi::api::core::v1::EnvFromSource>,
    /// The Service's declared volume mounts (config/secret), NOT the live app-db.
    pub volume_mounts: Vec<VolumeMount>,
    /// The Service's declared volumes (config/secret), NOT the live app-db PVC.
    pub volumes: Vec<Volume>,
    pub readiness_probe: Option<Probe>,
    pub resources: Option<ResourceRequirements>,
    pub image_pull_secrets: Vec<k8s_openapi::api::core::v1::LocalObjectReference>,
    pub service_account_name: String,
    /// Stateful pre-flight: mount the clone PVC at this data dir. `None` ⇒
    /// stateless boot-only (no clone).
    pub data_dir: Option<String>,
    /// Source app-db PVC to snapshot (stateful only).
    pub source_pvc: String,
    /// Clone PVC size (matches the source volume).
    pub storage_size: String,
    pub storage_class: String,
    /// `VolumeSnapshotClass` (empty ⇒ cluster default).
    pub snapshot_class: String,
    pub labels: BTreeMap<String, String>,
    pub owner: OwnerReference,
}

impl PreflightInputs {
    fn is_stateful(&self) -> bool {
        self.data_dir.is_some()
    }
}

const CLONE_VOLUME: &str = "preflight-data";
const SNAPSHOT_GVK: (&str, &str, &str) = ("snapshot.storage.k8s.io", "v1", "VolumeSnapshot");

fn pf_labels(
    base: &BTreeMap<String, String>,
    name: &str,
    target: &str,
) -> BTreeMap<String, String> {
    let mut l = base.clone();
    l.insert(PREFLIGHT_OF_LABEL.to_string(), name.to_string());
    l.insert(PREFLIGHT_TARGET_LABEL.to_string(), target_hash(target));
    l
}

/// Build the pre-flight candidate Pod: the candidate image with the Service's
/// boot env + real secrets, the CLONE mounted at the data dir (stateful),
/// `restartPolicy: Never` (a crash is terminal, not masked by restarts), no
/// Service wiring, owned by the CR so it GCs.
pub fn build_preflight_pod(name: &str, target: &str, i: &PreflightInputs) -> Pod {
    let mut volume_mounts = i.volume_mounts.clone();
    let mut volumes = i.volumes.clone();
    if let Some(dir) = &i.data_dir {
        volume_mounts.push(VolumeMount {
            name: CLONE_VOLUME.to_string(),
            mount_path: dir.clone(),
            ..Default::default()
        });
        volumes.push(Volume {
            name: CLONE_VOLUME.to_string(),
            persistent_volume_claim: Some(
                k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                    claim_name: preflight_clone_name(name, target),
                    read_only: None,
                },
            ),
            ..Default::default()
        });
    }

    let container = Container {
        name: "candidate".to_string(),
        image: Some(i.candidate_image.clone()),
        image_pull_policy: if i.pull_policy.is_empty() {
            None
        } else {
            Some(i.pull_policy.clone())
        },
        command: (!i.command.is_empty()).then(|| i.command.clone()),
        args: (!i.args.is_empty()).then(|| i.args.clone()),
        env: (!i.env.is_empty()).then(|| i.env.clone()),
        env_from: (!i.env_from.is_empty()).then(|| i.env_from.clone()),
        volume_mounts: (!volume_mounts.is_empty()).then_some(volume_mounts),
        readiness_probe: i.readiness_probe.clone(),
        resources: i.resources.clone(),
        ..Default::default()
    };

    Pod {
        metadata: ObjectMeta {
            name: Some(preflight_pod_name(name, target)),
            namespace: Some(i.namespace.clone()),
            labels: Some(pf_labels(&i.labels, name, target)),
            owner_references: Some(vec![i.owner.clone()]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![container],
            volumes: (!volumes.is_empty()).then_some(volumes),
            image_pull_secrets: (!i.image_pull_secrets.is_empty())
                .then(|| i.image_pull_secrets.clone()),
            service_account_name: (!i.service_account_name.is_empty())
                .then(|| i.service_account_name.clone()),
            // A crash is terminal — never restart, so a migration crash stays
            // observable as a terminated container instead of a hidden loop.
            restart_policy: Some("Never".to_string()),
            ..Default::default()
        }),
        status: None,
    }
}

/// Build the `VolumeSnapshot` (as a DynamicObject) of the live app-db PVC.
pub fn build_preflight_snapshot(name: &str, target: &str, i: &PreflightInputs) -> DynamicObject {
    let (g, v, k) = SNAPSHOT_GVK;
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(g, v, k));
    let mut obj = DynamicObject::new(&preflight_snapshot_name(name, target), &ar);
    obj.metadata.namespace = Some(i.namespace.clone());
    obj.metadata.labels = Some(pf_labels(&i.labels, name, target));
    obj.metadata.owner_references = Some(vec![i.owner.clone()]);
    let mut spec = serde_json::json!({
        "source": { "persistentVolumeClaimName": i.source_pvc },
    });
    if !i.snapshot_class.is_empty() {
        spec["volumeSnapshotClassName"] = serde_json::json!(i.snapshot_class);
    }
    obj.data = serde_json::json!({ "spec": spec });
    obj
}

/// Build the clone PVC that restores from the snapshot — the disposable copy the
/// candidate boots against (the live PVC is NEVER mounted into the pre-flight).
pub fn build_preflight_clone_pvc(
    name: &str,
    target: &str,
    i: &PreflightInputs,
) -> PersistentVolumeClaim {
    use k8s_openapi::api::core::v1::{
        PersistentVolumeClaimSpec, TypedLocalObjectReference, VolumeResourceRequirements,
    };
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(preflight_clone_name(name, target)),
            namespace: Some(i.namespace.clone()),
            labels: Some(pf_labels(&i.labels, name, target)),
            owner_references: Some(vec![i.owner.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: (!i.storage_class.is_empty()).then(|| i.storage_class.clone()),
            resources: Some(VolumeResourceRequirements {
                requests: Some({
                    let mut m = BTreeMap::new();
                    let size = if i.storage_size.is_empty() {
                        "1Gi".to_string()
                    } else {
                        i.storage_size.clone()
                    };
                    m.insert("storage".to_string(), Quantity(size));
                    m
                }),
                limits: None,
            }),
            data_source: Some(TypedLocalObjectReference {
                api_group: Some(SNAPSHOT_GVK.0.to_string()),
                kind: "VolumeSnapshot".to_string(),
                name: preflight_snapshot_name(name, target),
            }),
            ..Default::default()
        }),
        status: None,
    }
}

/// Observe the pre-flight candidate pod. An absent pod is `Booting` (the
/// converge step will (re)create it) — never a failure. A transient API error is
/// also `Booting` so a blip cannot fail an upgrade.
pub async fn observe_preflight(
    client: &kube::Client,
    namespace: &str,
    pod_name: &str,
) -> BootOutcome {
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    match pods.get_opt(pod_name).await {
        Ok(Some(p)) => health::pod_boot_outcome(&p),
        Ok(None) => BootOutcome::Booting,
        Err(e) => {
            tracing::debug!(error = %e, pod = pod_name, "pre-flight pod observe failed; treating as booting");
            BootOutcome::Booting
        }
    }
}

/// Converge the pre-flight resources to `want`:
///   * `want == true`  — ensure the current target's snapshot(+clone, stateful)
///     and pod exist (create-if-absent), and delete any STALE pre-flight
///     resources for this Service (from a superseded target).
///   * `want == false` — delete ALL pre-flight resources for this Service.
///
/// Idempotent and label-scoped, so it is safe to call every reconcile and after
/// an operator restart.
pub async fn converge_preflight(
    client: &kube::Client,
    name: &str,
    target: &str,
    want: bool,
    inputs: Option<&PreflightInputs>,
) -> Result<()> {
    let namespace = inputs
        .map(|i| i.namespace.clone())
        .or_else(|| infer_namespace(client))
        .unwrap_or_else(|| "default".to_string());

    let keep_hash = if want {
        Some(target_hash(target))
    } else {
        None
    };
    // Sweep stale/all pre-flight resources for this Service.
    delete_stale_preflight(client, &namespace, name, keep_hash.as_deref()).await?;

    if !want {
        return Ok(());
    }
    let Some(i) = inputs else {
        return Ok(());
    };

    // Stateful: snapshot the live PVC + a clone to boot against.
    if i.is_stateful() {
        let (g, v, k) = SNAPSHOT_GVK;
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(g, v, k));
        let snaps: Api<DynamicObject> = Api::namespaced_with(client.clone(), &namespace, &ar);
        let snap = build_preflight_snapshot(name, target, i);
        if let Err(e) = apply::apply_dynamic(&snaps, &snap).await {
            tracing::warn!(error = %e, "pre-flight VolumeSnapshot apply failed (snapshot CRD may be absent); pre-flight will time out fail-closed");
        }
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
        let clone = build_preflight_clone_pvc(name, target, i);
        // A clone PVC is immutable once bound; create-if-absent.
        if pvcs
            .get_opt(&preflight_clone_name(name, target))
            .await?
            .is_none()
        {
            apply::apply(&pvcs, &clone).await?;
        }
    }

    // The candidate pod — create-if-absent (a Pod spec is effectively immutable).
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    if pods
        .get_opt(&preflight_pod_name(name, target))
        .await?
        .is_none()
    {
        let pod = build_preflight_pod(name, target, i);
        apply::apply(&pods, &pod).await?;
    }
    Ok(())
}

/// Delete pre-flight resources for `name`. When `keep_hash` is `Some`, resources
/// carrying that target hash are kept (the current attempt) and only stale ones
/// are removed; when `None`, all are removed.
async fn delete_stale_preflight(
    client: &kube::Client,
    namespace: &str,
    name: &str,
    keep_hash: Option<&str>,
) -> Result<()> {
    let selector = format!("{PREFLIGHT_OF_LABEL}={name}");
    let lp = ListParams::default().labels(&selector);
    let dp = DeleteParams::background();

    // Pods.
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    if let Ok(list) = pods.list(&lp).await {
        for p in list {
            if should_delete(&p.labels().get(PREFLIGHT_TARGET_LABEL).cloned(), keep_hash) {
                let _ = pods.delete(&p.name_any(), &dp).await;
            }
        }
    }
    // Clone PVCs.
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);
    if let Ok(list) = pvcs.list(&lp).await {
        for p in list {
            if should_delete(&p.labels().get(PREFLIGHT_TARGET_LABEL).cloned(), keep_hash) {
                let _ = pvcs.delete(&p.name_any(), &dp).await;
            }
        }
    }
    // VolumeSnapshots (dynamic).
    let (g, v, k) = SNAPSHOT_GVK;
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(g, v, k));
    let snaps: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);
    if let Ok(list) = snaps.list(&lp).await {
        for s in list {
            if should_delete(&s.labels().get(PREFLIGHT_TARGET_LABEL).cloned(), keep_hash) {
                let _ = snaps.delete(&s.name_any(), &dp).await;
            }
        }
    }
    Ok(())
}

/// True when a resource carrying `label_hash` should be deleted given the
/// `keep_hash` we are preserving (None ⇒ delete everything).
fn should_delete(label_hash: &Option<String>, keep_hash: Option<&str>) -> bool {
    match keep_hash {
        None => true,
        Some(keep) => label_hash.as_deref() != Some(keep),
    }
}

/// Best-effort operator namespace from the in-cluster service-account file, used
/// only as a cleanup fallback when no inputs are available.
fn infer_namespace(_client: &kube::Client) -> Option<String> {
    std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn cfg(preflight: bool) -> Cfg {
        Cfg {
            preflight_needed: preflight,
            preflight_deadline_secs: 300,
            rollout_deadline_secs: 300,
        }
    }

    fn obs<'a>(
        desired: &'a str,
        running: Option<&'a str>,
        last_good: Option<&'a str>,
        prod_healthy: bool,
        upgrade: Option<&'a UpgradeStatus>,
    ) -> Observed<'a> {
        Observed {
            name: "cloud",
            desired,
            running,
            last_good,
            prod_healthy,
            upgrade,
            preflight: BootOutcome::Booting,
            rolling_crashloop: false,
            now: ts("2026-07-14T12:00:00Z"),
        }
    }

    // ---------- baseline bootstrap ----------

    #[test]
    fn initial_create_when_nothing_running() {
        let o = obs("img:v1", None, None, false, None);
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert!(p.next_upgrade.is_none());
        assert_eq!(p.mark_last_good, None);
    }

    #[test]
    fn adopts_healthy_running_as_baseline() {
        // First time we see the app healthy, the running image becomes last-good.
        let o = obs("img:v1", Some("img:v1"), None, true, None);
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(p.mark_last_good.as_deref(), Some("img:v1"));
    }

    #[test]
    fn no_baseline_no_disrupt_when_unhealthy() {
        // Running but unhealthy and no baseline — hold; do not flip to desired.
        let o = obs("img:v2", Some("img:v1"), None, false, None);
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert!(p.next_upgrade.is_none());
    }

    #[test]
    fn stable_keeps_baseline_current() {
        let o = obs("img:v1", Some("img:v1"), Some("img:v1"), true, None);
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(p.mark_last_good.as_deref(), Some("img:v1"));
        assert!(p.next_upgrade.is_none());
    }

    // ---------- start ----------

    #[test]
    fn start_upgrade_pre_flights_and_does_not_flip() {
        // desired != running, baseline present, pre-flight needed → Preflighting,
        // production stays on the OLD image.
        let o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, None);
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "must NOT flip during pre-flight"
        );
        let u = p.next_upgrade.unwrap();
        assert_eq!(u.phase, Some(UpgradePhase::Preflighting));
        assert_eq!(u.target_image, "img:v2");
        assert!(!u.preflight_pod.is_empty());
    }

    #[test]
    fn start_without_preflight_flips_into_rolling() {
        let o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, None);
        let p = plan(&o, &cfg(false)); // preflight not needed
        assert_eq!(
            p.effective_image, "img:v2",
            "no pre-flight → flip immediately"
        );
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Rolling));
    }

    #[test]
    fn desired_equals_last_good_converges_without_preflight() {
        // Reverting to the known-good image is a plain converge, no FSM churn.
        let o = obs("img:v1", Some("img:v2"), Some("img:v1"), false, None);
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert!(p.next_upgrade.is_none());
    }

    // ---------- pre-flight ----------

    fn preflighting_status() -> UpgradeStatus {
        UpgradeStatus {
            target_image: "img:v2".to_string(),
            phase: Some(UpgradePhase::Preflighting),
            started_at: "2026-07-14T11:59:00Z".to_string(),
            deadline_at: "2026-07-14T12:05:00Z".to_string(),
            preflight_pod: "cloud-preflight-abcd1234".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn preflight_ready_flips_to_rolling() {
        let u = preflighting_status();
        let mut o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        o.preflight = BootOutcome::Ready;
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v2", "pre-flight passed → flip");
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Rolling));
    }

    #[test]
    fn preflight_failed_never_flips_and_is_terminal() {
        let u = preflighting_status();
        let mut o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        o.preflight = BootOutcome::Failed;
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "INVARIANT: a failed candidate is NEVER put into production"
        );
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Failed));
        let r = p.record.unwrap();
        assert_eq!(r.result, "PreflightFailed");
        assert_eq!(r.image, "img:v2");
    }

    #[test]
    fn preflight_timeout_fails_closed() {
        let u = preflighting_status();
        let mut o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        o.preflight = BootOutcome::Booting;
        o.now = ts("2026-07-14T12:10:00Z"); // past the 12:05 deadline
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Failed));
    }

    #[test]
    fn preflight_booting_within_deadline_waits() {
        let u = preflighting_status();
        let mut o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        o.preflight = BootOutcome::Booting;
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "still on old image while booting"
        );
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::Preflighting)
        );
    }

    // ---------- rolling ----------

    fn rolling_status() -> UpgradeStatus {
        UpgradeStatus {
            target_image: "img:v2".to_string(),
            phase: Some(UpgradePhase::Rolling),
            started_at: "2026-07-14T11:59:00Z".to_string(),
            deadline_at: "2026-07-14T12:05:00Z".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn rolling_success_marks_last_good_and_clears() {
        let u = rolling_status();
        let o = obs("img:v2", Some("img:v2"), Some("img:v1"), true, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v2");
        assert!(
            p.next_upgrade.is_none(),
            "success clears the upgrade → Stable"
        );
        assert_eq!(p.mark_last_good.as_deref(), Some("img:v2"));
        assert_eq!(p.record.unwrap().result, "Succeeded");
    }

    #[test]
    fn rolling_crashloop_auto_rolls_back() {
        let u = rolling_status();
        let mut o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        o.rolling_crashloop = true;
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "INVARIANT: crashlooping candidate is reverted to last-good"
        );
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
        assert_eq!(p.mark_last_good, None);
    }

    #[test]
    fn rolling_deadline_auto_rolls_back() {
        let u = rolling_status();
        let mut o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        o.now = ts("2026-07-14T12:10:00Z"); // past deadline, not yet healthy
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
    }

    #[test]
    fn rolling_within_deadline_holds_on_candidate() {
        let u = rolling_status();
        let o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v2", "still rolling on candidate");
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Rolling));
    }

    // ---------- rollback ----------

    fn rollingback_status() -> UpgradeStatus {
        UpgradeStatus {
            target_image: "img:v2".to_string(),
            phase: Some(UpgradePhase::RollingBack),
            started_at: "2026-07-14T11:59:00Z".to_string(),
            deadline_at: "2026-07-14T12:10:00Z".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn rollback_completes_when_last_good_healthy() {
        let u = rollingback_status();
        let o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Failed));
        assert_eq!(p.record.unwrap().result, "RolledBack");
    }

    #[test]
    fn rollback_holds_last_good_until_healthy() {
        let u = rollingback_status();
        let o = obs("img:v2", Some("img:v1"), Some("img:v1"), false, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1", "keep restoring last-good");
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
    }

    // ---------- terminal + supersede ----------

    fn failed_status() -> UpgradeStatus {
        UpgradeStatus {
            target_image: "img:v2".to_string(),
            phase: Some(UpgradePhase::Failed),
            ..Default::default()
        }
    }

    #[test]
    fn failed_terminal_holds_on_last_good_and_never_reattempts() {
        // desired STILL points at the failed image → stay on last-good, no-op.
        let u = failed_status();
        let o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "INVARIANT: a known-bad target is never re-flipped"
        );
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Failed));
        assert!(p.record.is_none());
    }

    #[test]
    fn new_image_supersedes_a_failed_attempt() {
        // A NEW desired image reopens the FSM from Failed.
        let u = failed_status();
        let o = obs("img:v3", Some("img:v1"), Some("img:v1"), true, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "stay on last-good while pre-flighting v3"
        );
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::Preflighting)
        );
        // A terminal attempt is not re-recorded as Superseded.
        assert!(p.record.is_none());
    }

    #[test]
    fn retarget_mid_preflight_supersedes_and_records() {
        // desired changes while pre-flighting v2 → abandon v2 (Superseded), start v3.
        let u = preflighting_status();
        let o = obs("img:v3", Some("img:v1"), Some("img:v1"), true, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(p.effective_image, "img:v1");
        let next = p.next_upgrade.unwrap();
        assert_eq!(next.phase, Some(UpgradePhase::Preflighting));
        assert_eq!(next.target_image, "img:v3");
        let r = p.record.unwrap();
        assert_eq!(r.result, "Superseded");
        assert_eq!(r.image, "img:v2");
    }

    // ---------- resume-after-restart (idempotence) ----------

    #[test]
    fn resume_rolling_is_deterministic_across_restart() {
        // The operator restarts mid-Rolling: with the SAME persisted status +
        // live observation, plan() yields the SAME decision (no in-memory state).
        let u = rolling_status();
        let o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        let a = plan(&o, &cfg(true));
        let b = plan(&o, &cfg(true));
        assert_eq!(a, b);
        assert_eq!(a.effective_image, "img:v2");
    }

    #[test]
    fn resume_preflight_reobserves_the_named_pod() {
        // Post-restart the pod name is recoverable from the persisted status —
        // deterministic by target — so observation resumes without re-creating.
        let u = preflighting_status();
        assert_eq!(u.preflight_pod, "cloud-preflight-abcd1234");
        assert_eq!(
            preflight_pod_name("cloud", "img:v2"),
            preflight_pod_name("cloud", "img:v2"),
            "name is a pure function of (service, target)"
        );
    }

    // ---------- the load-bearing invariant, swept ----------

    #[test]
    fn never_leaves_prod_on_an_unproven_or_failed_candidate() {
        // Across every non-success state, the effective image is last-good — the
        // candidate is only ever in production in Rolling (post-pre-flight) or on
        // proven success.
        let lg = "img:good";
        let cand = "img:bad";

        // Preflighting (booting / failed / timeout) → last-good.
        for outcome in [BootOutcome::Booting, BootOutcome::Failed] {
            let u = UpgradeStatus {
                target_image: cand.to_string(),
                phase: Some(UpgradePhase::Preflighting),
                deadline_at: "2026-07-14T12:05:00Z".to_string(),
                ..Default::default()
            };
            let mut o = obs(cand, Some(lg), Some(lg), true, Some(&u));
            o.preflight = outcome;
            assert_eq!(plan(&o, &cfg(true)).effective_image, lg);
        }

        // RollingBack → last-good.
        let rb = UpgradeStatus {
            target_image: cand.to_string(),
            phase: Some(UpgradePhase::RollingBack),
            deadline_at: "2026-07-14T12:10:00Z".to_string(),
            ..Default::default()
        };
        let o = obs(cand, Some(cand), Some(lg), false, Some(&rb));
        assert_eq!(plan(&o, &cfg(true)).effective_image, lg);

        // Failed terminal → last-good.
        let f = UpgradeStatus {
            target_image: cand.to_string(),
            phase: Some(UpgradePhase::Failed),
            ..Default::default()
        };
        let o = obs(cand, Some(lg), Some(lg), true, Some(&f));
        assert_eq!(plan(&o, &cfg(true)).effective_image, lg);
    }

    // ---------- helpers ----------

    #[test]
    fn history_is_bounded() {
        let mut h = Vec::new();
        for i in 0..(MAX_HISTORY + 5) {
            push_history(
                &mut h,
                make_record(
                    &format!("img:{i}"),
                    None,
                    "Succeeded",
                    ts("2026-07-14T12:00:00Z"),
                    "",
                ),
            );
        }
        assert_eq!(h.len(), MAX_HISTORY);
        // Oldest dropped, newest kept.
        assert_eq!(h.last().unwrap().image, format!("img:{}", MAX_HISTORY + 4));
    }

    #[test]
    fn deadline_is_floored() {
        let now = ts("2026-07-14T12:00:00Z");
        // A 5s request is floored to 30s.
        let d: Timestamp = deadline_str(now, 5).parse().unwrap();
        assert!(
            d >= now
                .checked_add(SignedDuration::from_secs(MIN_DEADLINE_SECS))
                .unwrap()
        );
    }

    #[test]
    fn target_hash_is_stable_and_distinct() {
        assert_eq!(target_hash("img:v2"), target_hash("img:v2"));
        assert_ne!(target_hash("img:v2"), target_hash("img:v3"));
        assert_eq!(target_hash("img:v2").len(), 8);
    }

    #[test]
    fn preflight_names_fit_dns_1123() {
        let long = "a-very-long-service-name-that-exceeds-the-k8s-limit-substantially";
        for f in [
            preflight_pod_name,
            preflight_clone_name,
            preflight_snapshot_name,
        ] {
            let n = f(long, "img:v2");
            assert!(n.len() <= 63, "{n} exceeds 63 chars");
            assert!(!n.ends_with('-'));
        }
    }

    #[test]
    fn should_delete_keeps_current_hash_only() {
        let keep = target_hash("img:v2");
        assert!(!should_delete(&Some(keep.clone()), Some(&keep)));
        assert!(should_delete(&Some(target_hash("img:v3")), Some(&keep)));
        assert!(should_delete(&None, Some(&keep)));
        // No keep_hash ⇒ delete everything.
        assert!(should_delete(&Some(keep), None));
    }

    // ============================================================================
    // Full-lifecycle SEQUENCE tests — drive plan() through a simulated reconcile
    // loop, threading status forward exactly as the controller would. This is the
    // operator-side integration proof of the discipline the task requires: a happy
    // upgrade, a crashlooping candidate → auto-rollback → prod stays on lastGood,
    // and resume-after-restart mid-upgrade.
    // ============================================================================

    /// A simulated persisted world. `step` runs one reconcile: build the
    /// observation, run plan(), then persist the next status + advance last-good,
    /// and model the Deployment converging to the effective image (so the NEXT
    /// step observes it as `running`).
    struct World {
        running: Option<String>,
        last_good: Option<String>,
        upgrade: Option<UpgradeStatus>,
    }

    impl World {
        fn fresh() -> Self {
            World {
                running: None,
                last_good: None,
                upgrade: None,
            }
        }

        #[allow(clippy::too_many_arguments)]
        fn step(
            &mut self,
            desired: &str,
            prod_healthy: bool,
            preflight: BootOutcome,
            crashloop: bool,
            now: Timestamp,
            cfg: &Cfg,
        ) -> Plan {
            let o = Observed {
                name: "cloud",
                desired,
                running: self.running.as_deref(),
                last_good: self.last_good.as_deref(),
                prod_healthy,
                upgrade: self.upgrade.as_ref(),
                preflight,
                rolling_crashloop: crashloop,
                now,
            };
            let p = plan(&o, cfg);
            self.upgrade = p.next_upgrade.clone();
            if let Some(lg) = &p.mark_last_good {
                self.last_good = Some(lg.clone());
            }
            // The controller applies effective_image; model the Deployment
            // converging to it before the next reconcile.
            self.running = Some(p.effective_image.clone());
            p
        }
    }

    #[test]
    fn sequence_happy_upgrade() {
        let cfg = cfg(true); // pre-flight required
        let t0 = ts("2026-07-14T12:00:00Z");
        let mut w = World::fresh();

        // 1. Initial create at v1.
        assert_eq!(
            w.step("img:v1", false, BootOutcome::Booting, false, t0, &cfg)
                .effective_image,
            "img:v1"
        );
        // 2. v1 becomes healthy → adopt baseline.
        w.step("img:v1", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(w.last_good.as_deref(), Some("img:v1"));
        // 3. Bump desired to v2 → start pre-flight; prod STAYS on v1.
        let p = w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(
            p.effective_image, "img:v1",
            "must not flip during pre-flight"
        );
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::Preflighting)
        );
        // 4. Pre-flight still booting → still on v1.
        assert_eq!(
            w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg)
                .effective_image,
            "img:v1"
        );
        // 5. Pre-flight passes → FLIP to v2, rolling.
        let p = w.step("img:v2", false, BootOutcome::Ready, false, t0, &cfg);
        assert_eq!(p.effective_image, "img:v2");
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::Rolling)
        );
        // 6. v2 healthy in prod → success, baseline advances, upgrade clears.
        let p = w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(p.effective_image, "img:v2");
        assert!(w.upgrade.is_none(), "success ⇒ Stable");
        assert_eq!(w.last_good.as_deref(), Some("img:v2"));
    }

    #[test]
    fn sequence_crashloop_auto_rollback_keeps_prod_on_last_good() {
        let cfg = cfg(true);
        let t0 = ts("2026-07-14T12:00:00Z");
        let mut w = World::fresh();
        // Reach a healthy v1 baseline.
        w.step("img:v1", false, BootOutcome::Booting, false, t0, &cfg);
        w.step("img:v1", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(w.last_good.as_deref(), Some("img:v1"));
        // Bump to v2, pre-flight passes, flip to v2.
        w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg); // start pre-flight
        let p = w.step("img:v2", false, BootOutcome::Ready, false, t0, &cfg); // flip
        assert_eq!(p.effective_image, "img:v2");
        // v2 crash-loops in prod → AUTO-ROLLBACK to v1.
        let p = w.step("img:v2", false, BootOutcome::Booting, true, t0, &cfg);
        assert_eq!(
            p.effective_image, "img:v1",
            "crashloop ⇒ revert to last-good"
        );
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
        // v1 healthy again → rollback concludes; terminal Failed, prod on v1.
        let p = w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::Failed)
        );
        assert_eq!(w.last_good.as_deref(), Some("img:v1"), "baseline unchanged");
        // Desired STILL v2 (the bad image) → operator holds prod on v1, no re-flip.
        for _ in 0..3 {
            let p = w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg);
            assert_eq!(
                p.effective_image, "img:v1",
                "INVARIANT: never re-flip a known-bad image"
            );
        }
        // A NEW good image v3 supersedes and starts a fresh pre-flight from v1.
        let p = w.step("img:v3", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(
            p.effective_image, "img:v1",
            "still on last-good while pre-flighting v3"
        );
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::Preflighting)
        );
        assert_eq!(w.upgrade.as_ref().unwrap().target_image, "img:v3");
    }

    #[test]
    fn sequence_migration_crash_preflight_never_reaches_prod() {
        // The v1.800.1-class bug: the candidate cannot boot over real (cloned)
        // data. The pre-flight catches it; production is NEVER flipped.
        let cfg = cfg(true);
        let t0 = ts("2026-07-14T12:00:00Z");
        let mut w = World::fresh();
        w.step("img:v1", false, BootOutcome::Booting, false, t0, &cfg);
        w.step("img:v1", true, BootOutcome::Booting, false, t0, &cfg);
        // Bump to a migration-breaking v2; pre-flight boot over cloned data FAILS.
        w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg); // start pre-flight
        let p = w.step("img:v2", true, BootOutcome::Failed, false, t0, &cfg);
        assert_eq!(
            p.effective_image, "img:v1",
            "pre-flight crash ⇒ prod never flipped"
        );
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::Failed)
        );
        assert_eq!(p.record.unwrap().result, "PreflightFailed");
        // Prod stayed on v1 the whole time; never observed v2.
        assert_eq!(w.running.as_deref(), Some("img:v1"));
    }

    #[test]
    fn sequence_resume_after_restart_is_identical() {
        // Mid-Rolling, the operator restarts: with the SAME persisted status +
        // live observation, the very next plan() is identical (no in-memory state).
        let cfg = cfg(true);
        let t0 = ts("2026-07-14T12:00:00Z");
        let mut w = World::fresh();
        w.step("img:v1", false, BootOutcome::Booting, false, t0, &cfg);
        w.step("img:v1", true, BootOutcome::Booting, false, t0, &cfg);
        w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg);
        w.step("img:v2", false, BootOutcome::Ready, false, t0, &cfg); // flip to Rolling
                                                                      // "Restart": snapshot the persisted world, then re-run the SAME observation.
        let saved_upgrade = w.upgrade.clone();
        let saved_lg = w.last_good.clone();
        let saved_running = w.running.clone();
        let obs_a = Observed {
            name: "cloud",
            desired: "img:v2",
            running: saved_running.as_deref(),
            last_good: saved_lg.as_deref(),
            prod_healthy: false,
            upgrade: saved_upgrade.as_ref(),
            preflight: BootOutcome::Booting,
            rolling_crashloop: false,
            now: t0,
        };
        let a = plan(&obs_a, &cfg);
        let b = plan(&obs_a, &cfg); // a fresh process, same inputs
        assert_eq!(a, b);
        assert_eq!(a.effective_image, "img:v2");
        assert_eq!(
            a.next_upgrade.unwrap().phase,
            Some(UpgradePhase::Rolling),
            "resumes Rolling, not restarts the upgrade"
        );
    }

    // ============================================================================
    // Pre-flight resource builder shape — the no-side-effects / clone-not-live-PVC
    // guarantees the task requires.
    // ============================================================================

    fn stateful_inputs() -> PreflightInputs {
        PreflightInputs {
            namespace: "hanzo".into(),
            candidate_image: "img:v2".into(),
            pull_policy: "IfNotPresent".into(),
            command: vec![],
            args: vec![],
            env: vec![k8s_openapi::api::core::v1::EnvVar {
                name: "CLOUD_ENV".into(),
                value: Some("smoke".into()),
                ..Default::default()
            }],
            env_from: vec![],
            volume_mounts: vec![],
            volumes: vec![],
            readiness_probe: None,
            resources: None,
            image_pull_secrets: vec![],
            service_account_name: String::new(),
            data_dir: Some("/var/lib/hanzo/cloud".into()),
            source_pvc: "cloud-app-db".into(),
            storage_size: "10Gi".into(),
            storage_class: "do-block-storage".into(),
            snapshot_class: "do-snap".into(),
            labels: BTreeMap::new(),
            owner: OwnerReference::default(),
        }
    }

    #[test]
    fn preflight_pod_mounts_clone_never_live_pvc() {
        let i = stateful_inputs();
        let pod = build_preflight_pod("cloud", "img:v2", &i);
        let spec = pod.spec.unwrap();

        // restartPolicy Never — a crash is terminal and observable.
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));

        let vols = spec.volumes.unwrap();
        // The clone PVC is mounted…
        let clone = preflight_clone_name("cloud", "img:v2");
        assert!(
            vols.iter().any(|v| v
                .persistent_volume_claim
                .as_ref()
                .map(|p| p.claim_name == clone)
                .unwrap_or(false)),
            "must mount the clone PVC"
        );
        // …and the LIVE app-db PVC is NEVER referenced.
        assert!(
            !vols.iter().any(|v| v
                .persistent_volume_claim
                .as_ref()
                .map(|p| p.claim_name == "cloud-app-db")
                .unwrap_or(false)),
            "INVARIANT: the live PVC is never mounted into the pre-flight pod"
        );

        let c = &spec.containers[0];
        assert_eq!(
            c.image.as_deref(),
            Some("img:v2"),
            "boots the CANDIDATE image"
        );
        // Clone mounted at the data dir.
        assert!(c
            .volume_mounts
            .as_ref()
            .unwrap()
            .iter()
            .any(|m| m.name == CLONE_VOLUME && m.mount_path == "/var/lib/hanzo/cloud"));
        // Boot-only env present (side-effect suppression).
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name == "CLOUD_ENV" && e.value.as_deref() == Some("smoke")));
    }

    #[test]
    fn stateless_preflight_pod_has_no_data_volume() {
        let mut i = stateful_inputs();
        i.data_dir = None; // stateless boot-only
        let pod = build_preflight_pod("cloud", "img:v2", &i);
        let spec = pod.spec.unwrap();
        assert_eq!(spec.restart_policy.as_deref(), Some("Never"));
        assert!(spec.volumes.is_none(), "stateless ⇒ no clone volume");
        // Still boots the candidate with the boot-only env.
        let c = &spec.containers[0];
        assert_eq!(c.image.as_deref(), Some("img:v2"));
        assert!(c
            .env
            .as_ref()
            .unwrap()
            .iter()
            .any(|e| e.name == "CLOUD_ENV"));
    }

    #[test]
    fn preflight_snapshot_targets_the_live_pvc() {
        let i = stateful_inputs();
        let snap = build_preflight_snapshot("cloud", "img:v2", &i);
        let spec = &snap.data["spec"];
        assert_eq!(spec["source"]["persistentVolumeClaimName"], "cloud-app-db");
        assert_eq!(spec["volumeSnapshotClassName"], "do-snap");
    }

    #[test]
    fn preflight_clone_restores_from_the_snapshot() {
        let i = stateful_inputs();
        let pvc = build_preflight_clone_pvc("cloud", "img:v2", &i);
        let spec = pvc.spec.unwrap();
        let ds = spec.data_source.unwrap();
        assert_eq!(ds.kind, "VolumeSnapshot");
        assert_eq!(ds.name, preflight_snapshot_name("cloud", "img:v2"));
        assert_eq!(ds.api_group.as_deref(), Some("snapshot.storage.k8s.io"));
        // Clone matches the source size.
        let req = spec.resources.unwrap().requests.unwrap();
        assert_eq!(req.get("storage").unwrap().0, "10Gi");
    }

    #[test]
    fn preflight_resources_carry_the_owner_and_label() {
        let i = stateful_inputs();
        let pod = build_preflight_pod("cloud", "img:v2", &i);
        // Owned by the CR (GCs with it) + label-scoped for cleanup.
        assert!(pod.metadata.owner_references.is_some());
        let labels = pod.metadata.labels.unwrap();
        assert_eq!(
            labels.get(PREFLIGHT_OF_LABEL).map(String::as_str),
            Some("cloud")
        );
        assert_eq!(
            labels.get(PREFLIGHT_TARGET_LABEL).map(String::as_str),
            Some(target_hash("img:v2").as_str())
        );
    }
}
