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
use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicySpec};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
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
/// Default soak window: the candidate must stay continuously healthy in
/// production for this long AFTER the rollout completes before the upgrade is
/// committed (Succeeded + `lastGoodImage` advanced). Catches a candidate that
/// rolls out healthy then crash-loops under load. `0` opts out.
pub const DEFAULT_SOAK_SECS: i64 = 60;
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
    /// Seconds of continuous production health the candidate must hold after the
    /// rollout completes before the upgrade commits. `0` ⇒ commit on first
    /// healthy observation.
    pub soak_seconds: i64,
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

/// Drive the Rolling phase: the candidate must roll out healthy AND stay healthy
/// through a soak window before the upgrade commits; auto-rollback on crashloop
/// or deadline. The soak (MED-3) catches a candidate that rolls out healthy then
/// crash-loops under load — it is rolled back instead of poisoning last-good.
fn drive_rolling(obs: &Observed, cfg: &Cfg, u: &UpgradeStatus) -> Plan {
    let target = u.target_image.clone();

    // The candidate is healthy on the Deployment. Require a soak window of
    // CONTINUOUS health (anchored at the first healthy observation) before
    // committing — the deadline no longer applies once healthy (there is no
    // downtime to bound), only crashloop can still roll it back.
    if obs.running == Some(target.as_str()) && obs.prod_healthy {
        let anchor = soak_anchor(u, obs.now);
        if soaked(anchor, obs.now, cfg.soak_seconds) {
            return Plan {
                effective_image: target.clone(),
                next_upgrade: None, // Stable
                record: Some(make_record(
                    &target,
                    obs.last_good,
                    "Succeeded",
                    obs.now,
                    "candidate rolled out healthy and held through the soak window",
                )),
                mark_last_good: Some(target), // advance baseline — proven healthy + stable
                reason: "rollout healthy through soak; upgrade succeeded",
            };
        }
        // Healthy but the soak window has not yet elapsed — hold on the candidate,
        // anchoring (or preserving) the soak start.
        let mut next = u.clone();
        next.stable_since = anchor.to_string();
        next.message = "candidate healthy; soaking before commit".to_string();
        return Plan {
            effective_image: target,
            next_upgrade: Some(next),
            record: None,
            mark_last_good: None,
            reason: "candidate healthy; soaking before commit",
        };
    }

    // Not healthy on the candidate. Auto-rollback on crashloop (fast path) or when
    // the deadline to reach a healthy state has passed.
    if obs.rolling_crashloop || expired(u, obs.now) {
        let reason = if obs.rolling_crashloop {
            "candidate crash-looping; auto-rolling back to last-good"
        } else {
            "rollout deadline exceeded; auto-rolling back to last-good"
        };
        // The rollback target MUST be the proven baseline — NEVER the candidate
        // (flipping onto the very image that is failing is not a rollback). A
        // Rolling phase is only reachable with a baseline; an absent `last_good`
        // is a corrupt status, so fail CLOSED to terminal rather than pick the
        // candidate as a fake recovery target (LOW-5).
        return match obs.last_good {
            Some(lg) => Plan {
                effective_image: lg.to_string(), // FLIP BACK to last-good
                next_upgrade: Some(rolling_back(&target, obs.now, cfg, &u.started_at)),
                record: None,
                mark_last_good: None,
                reason,
            },
            None => halt_no_baseline(obs, u, reason),
        };
    }

    // Still rolling within the deadline, not yet healthy — clear any soak anchor
    // (a candidate that reached health then fell back must re-soak from scratch).
    let mut next = u.clone();
    next.stable_since = String::new();
    next.message = "health-gating rollout".to_string();
    Plan {
        effective_image: target,
        next_upgrade: Some(next),
        record: None,
        mark_last_good: None,
        reason: "health-gating rollout",
    }
}

/// The soak-window start instant: the persisted `stable_since` if present and
/// parseable, else `now` (the first healthy observation, or a re-anchor when the
/// stored value is corrupt — fail-safe: a corrupt anchor forces a fresh full
/// soak rather than a premature commit).
fn soak_anchor(u: &UpgradeStatus, now: Timestamp) -> Timestamp {
    if u.stable_since.is_empty() {
        now
    } else {
        u.stable_since.parse::<Timestamp>().unwrap_or(now)
    }
}

/// True when the candidate has been continuously healthy since `anchor` for at
/// least `soak_secs`. `soak_secs <= 0` ⇒ commit immediately (soak opted out).
fn soaked(anchor: Timestamp, now: Timestamp, soak_secs: i64) -> bool {
    now.duration_since(anchor).as_secs() >= soak_secs.max(0)
}

/// A crashloop/deadline fired in Rolling (or a rollback was due in RollingBack)
/// but the status carries NO `last_good` baseline — a corrupt or hand-edited
/// status (both phases are only reachable with a baseline). There is no
/// known-good image to roll back to, so fail CLOSED: land in terminal Failed and
/// hold whatever prod currently runs, WITHOUT flipping onto the candidate as a
/// fake rollback target and WITHOUT marking anything last-good. A human must
/// intervene; the FSM does not oscillate.
fn halt_no_baseline(obs: &Observed, u: &UpgradeStatus, reason: &'static str) -> Plan {
    let hold = obs
        .running
        .or(obs.last_good)
        .unwrap_or(obs.desired)
        .to_string();
    Plan {
        effective_image: hold,
        next_upgrade: Some(failed(
            &u.target_image,
            "no last-good baseline to roll back to (corrupt status); upgrade halted",
        )),
        record: Some(make_record(
            &u.target_image,
            obs.last_good,
            "RolledBack",
            obs.now,
            "no last-good baseline; upgrade halted without flipping onto the candidate",
        )),
        mark_last_good: None,
        reason,
    }
}

/// Drive the RollingBack phase: hold the Deployment on last-good until it is
/// healthy again, then conclude in the terminal Failed state.
fn drive_rolling_back(obs: &Observed, u: &UpgradeStatus) -> Plan {
    // The rollback target is the proven baseline — NEVER the candidate (LOW-5).
    // RollingBack is only reachable with a baseline; an absent `last_good` is a
    // corrupt status, so fail CLOSED rather than "roll back" onto the candidate.
    let Some(lg) = obs.last_good else {
        return halt_no_baseline(
            obs,
            u,
            "rolling back but no last-good baseline (corrupt status)",
        );
    };
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
    // Hold on last-good. NEVER re-select the failed candidate as the hold image
    // (LOW-5): fall back to the current running image only when it is not the
    // candidate, else to desired.
    let stay = obs
        .last_good
        .or_else(|| obs.running.filter(|r| *r != u.target_image.as_str()))
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
/// treated as EXPIRED — a safety FSM fails CLOSED (LOW-6): a corrupt deadline
/// rolls the candidate back rather than letting it run unbounded. In normal
/// operation every Preflighting/Rolling status carries a valid deadline set by
/// `deadline_str`, so this only bites a hand-edited/corrupt status.
fn expired(u: &UpgradeStatus, now: Timestamp) -> bool {
    match u.deadline_at.parse::<Timestamp>() {
        Ok(deadline) => now > deadline,
        Err(_) => true,
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
        stable_since: String::new(),
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

/// Refuse an unsafe managed-upgrade configuration BEFORE any pre-flight runs
/// (HIGH-1). Pure over the spec-derived values so the guard is unit-tested
/// without a cluster. `Some(reason)` ⇒ the controller holds production on the
/// current image and does not create the pre-flight pod; `None` ⇒ the upgrade
/// may proceed.
///
/// Two refusals, checked in order:
///   1. `spec.volumes` declares the live app-db PVC — it would be carried
///      READ-WRITE into the pre-flight pod, defeating the clone-not-live
///      isolation (the pre-flight must boot a snapshot CLONE, never live data).
///   2. A stateful upgrade with no `bootEnv` — the pre-flight boots the real
///      image with the real master key (`envFrom`) over a clone of live data;
///      without a boot-only marker to suppress side effects it would run live
///      migrations / notifications / billing. The deny-all-egress NetworkPolicy
///      is the containment; `bootEnv` is the required belt-and-braces.
pub fn upgrade_refusal(
    stateful: bool,
    boot_env_empty: bool,
    declares_live_app_db: bool,
) -> Option<&'static str> {
    if declares_live_app_db {
        return Some(
            "spec.volumes declares the live app-db PVC; a managed-upgrade pre-flight must boot a snapshot CLONE of the data, never the live volume",
        );
    }
    if stateful && boot_env_empty {
        return Some(
            "stateful managed upgrade requires spec.upgradePolicy.bootEnv (a boot-only marker, e.g. CLOUD_ENV=smoke) so the pre-flight suppresses live side effects",
        );
    }
    None
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

pub fn preflight_netpol_name(name: &str, target: &str) -> String {
    pf_name(name, target, "-egress")
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

/// Match labels selecting exactly the one pre-flight pod for `(name, target)`.
/// The two pre-flight labels are unique to the pre-flight pod — production pods
/// never carry them — so a policy selecting on them never touches a live
/// workload (even though the pod also shares the app's `standard_labels`).
fn preflight_pod_match_labels(name: &str, target: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(PREFLIGHT_OF_LABEL.to_string(), name.to_string());
    m.insert(PREFLIGHT_TARGET_LABEL.to_string(), target_hash(target));
    m
}

/// Build the deny-all-egress `NetworkPolicy` scoping the pre-flight candidate pod
/// (HIGH-1). The CSI clone isolates the pod's DATA, but the candidate boots the
/// real image with the real master key (`envFrom`) and the real ServiceAccount —
/// everything reached over the network is LIVE (external-DB migrations,
/// replicate/S3 push, KMS writes, IAM registration, notifications/billing/
/// webhooks). A boot-to-ready check needs no egress, so this policy selects the
/// pre-flight pod (by its unique pre-flight labels, never a production pod) and
/// denies ALL egress. The kubelet readiness probe is ingress from the node and
/// is unaffected, so the boot check still works. Owned + labelled like the
/// pod/clone/snapshot so `converge_preflight` creates and sweeps it identically.
pub fn build_preflight_netpol(name: &str, target: &str, i: &PreflightInputs) -> NetworkPolicy {
    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(preflight_netpol_name(name, target)),
            namespace: Some(i.namespace.clone()),
            labels: Some(pf_labels(&i.labels, name, target)),
            owner_references: Some(vec![i.owner.clone()]),
            ..Default::default()
        },
        spec: Some(NetworkPolicySpec {
            pod_selector: Some(LabelSelector {
                match_labels: Some(preflight_pod_match_labels(name, target)),
                ..Default::default()
            }),
            // Egress isolation with an EMPTY egress rule set ⇒ deny ALL egress.
            policy_types: Some(vec!["Egress".to_string()]),
            egress: Some(vec![]),
            ..Default::default()
        }),
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
    namespace: &str,
    name: &str,
    target: &str,
    want: bool,
    inputs: Option<&PreflightInputs>,
) -> Result<()> {
    let keep_hash = if want {
        Some(target_hash(target))
    } else {
        None
    };
    // Sweep stale/all pre-flight resources for this Service (pod, clone, snapshot,
    // egress NetworkPolicy). When `want == false` this is the disable/inactive
    // sweep (MED-4): everything for this Service is removed by label.
    delete_stale_preflight(client, namespace, name, keep_hash.as_deref()).await?;

    if !want {
        return Ok(());
    }
    let Some(i) = inputs else {
        return Ok(());
    };

    // Deny-all-egress NetworkPolicy FIRST so egress is denied before the candidate
    // pod is admitted (HIGH-1: no window where the booting candidate has egress).
    let nps: Api<NetworkPolicy> = Api::namespaced(client.clone(), namespace);
    if nps
        .get_opt(&preflight_netpol_name(name, target))
        .await?
        .is_none()
    {
        let np = build_preflight_netpol(name, target, i);
        apply::apply(&nps, &np).await?;
    }

    // Stateful: snapshot the live PVC + a clone to boot against.
    if i.is_stateful() {
        let (g, v, k) = SNAPSHOT_GVK;
        let ar = ApiResource::from_gvk(&GroupVersionKind::gvk(g, v, k));
        let snaps: Api<DynamicObject> = Api::namespaced_with(client.clone(), namespace, &ar);
        let snap = build_preflight_snapshot(name, target, i);
        if let Err(e) = apply::apply_dynamic(&snaps, &snap).await {
            tracing::warn!(error = %e, "pre-flight VolumeSnapshot apply failed (snapshot CRD may be absent); pre-flight will time out fail-closed");
        }
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);
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
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
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
    // Deny-egress NetworkPolicies.
    let nps: Api<NetworkPolicy> = Api::namespaced(client.clone(), namespace);
    if let Ok(list) = nps.list(&lp).await {
        for np in list {
            if should_delete(&np.labels().get(PREFLIGHT_TARGET_LABEL).cloned(), keep_hash) {
                let _ = nps.delete(&np.name_any(), &dp).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn cfg(preflight: bool) -> Cfg {
        // Soak disabled in the transition/sequence fixtures — those assert the
        // state machine's transitions, not the soak window (which has its own
        // tests via `cfg_soak`). Soak=0 ⇒ commit on first healthy observation.
        Cfg {
            preflight_needed: preflight,
            preflight_deadline_secs: 300,
            rollout_deadline_secs: 300,
            soak_seconds: 0,
        }
    }

    fn cfg_soak(preflight: bool, soak_seconds: i64) -> Cfg {
        Cfg {
            preflight_needed: preflight,
            preflight_deadline_secs: 300,
            rollout_deadline_secs: 300,
            soak_seconds,
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

    // ---------- soak window (MED-3): no last-good poisoning ----------

    #[test]
    fn rolling_healthy_but_unsoaked_holds_and_anchors_soak() {
        // First healthy observation with a soak window: hold on the candidate and
        // record the soak anchor — NOT yet Succeeded, last-good NOT advanced.
        let u = rolling_status();
        let o = obs("img:v2", Some("img:v2"), Some("img:v1"), true, Some(&u));
        let p = plan(&o, &cfg_soak(true, 60));
        assert_eq!(p.effective_image, "img:v2", "still on the candidate");
        let next = p.next_upgrade.unwrap();
        assert_eq!(
            next.phase,
            Some(UpgradePhase::Rolling),
            "stays Rolling during the soak"
        );
        assert!(!next.stable_since.is_empty(), "soak anchor recorded");
        assert_eq!(
            p.mark_last_good, None,
            "last-good is NOT advanced before the soak elapses"
        );
        assert!(p.record.is_none());
    }

    #[test]
    fn rolling_soak_complete_succeeds_and_marks_last_good() {
        // Continuously healthy for 65s with a 60s soak → committed.
        let mut u = rolling_status();
        u.stable_since = "2026-07-14T11:59:30Z".to_string();
        let mut o = obs("img:v2", Some("img:v2"), Some("img:v1"), true, Some(&u));
        o.now = ts("2026-07-14T12:00:35Z"); // 65s of continuous health
        let p = plan(&o, &cfg_soak(true, 60));
        assert_eq!(p.effective_image, "img:v2");
        assert!(p.next_upgrade.is_none(), "soak complete ⇒ Stable");
        assert_eq!(p.mark_last_good.as_deref(), Some("img:v2"));
        assert_eq!(p.record.unwrap().result, "Succeeded");
    }

    #[test]
    fn rolling_unhealthy_resets_the_soak_anchor() {
        // A candidate that reached health (anchor set) then drops unhealthy (not a
        // crashloop, within the deadline) must re-soak from scratch.
        let mut u = rolling_status();
        u.stable_since = "2026-07-14T11:59:30Z".to_string();
        let o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        let p = plan(&o, &cfg_soak(true, 60));
        assert_eq!(
            p.effective_image, "img:v2",
            "still rolling on the candidate"
        );
        let next = p.next_upgrade.unwrap();
        assert_eq!(next.phase, Some(UpgradePhase::Rolling));
        assert!(
            next.stable_since.is_empty(),
            "soak anchor reset on an unhealthy observation"
        );
        assert_eq!(p.mark_last_good, None);
    }

    #[test]
    fn rolling_crash_during_soak_rolls_back_and_never_marks_good() {
        // THE MED-3 property: a candidate that rolled out healthy (anchor set) then
        // crash-loops under load DURING the soak is rolled back to last-good and is
        // NEVER marked good — no last-good poisoning.
        let mut u = rolling_status();
        u.stable_since = "2026-07-14T11:59:30Z".to_string();
        let mut o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        o.rolling_crashloop = true;
        let p = plan(&o, &cfg_soak(true, 60));
        assert_eq!(
            p.effective_image, "img:v1",
            "INVARIANT: a crash during soak reverts to last-good"
        );
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
        assert_eq!(
            p.mark_last_good, None,
            "a briefly-healthy candidate that crashed is NEVER marked good"
        );
    }

    #[test]
    fn rolling_healthy_past_deadline_soaks_rather_than_rolling_back() {
        // Once healthy, the rollout deadline no longer forces a rollback (there is
        // no downtime to bound) — only a crashloop can. A healthy candidate still
        // mid-soak past the deadline keeps soaking.
        let mut u = rolling_status(); // deadline 12:05:00
        u.stable_since = "2026-07-14T12:04:50Z".to_string();
        let mut o = obs("img:v2", Some("img:v2"), Some("img:v1"), true, Some(&u));
        o.now = ts("2026-07-14T12:05:10Z"); // past the deadline, healthy 20s, soak 60
        let p = plan(&o, &cfg_soak(true, 60));
        assert_eq!(
            p.effective_image, "img:v2",
            "healthy candidate past the deadline keeps soaking, NOT rolled back"
        );
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Rolling));
        assert_eq!(p.mark_last_good, None);
    }

    // ---------- corrupt status: never roll back onto the candidate (LOW-5) ----------

    #[test]
    fn rolling_crashloop_with_no_baseline_halts_terminal_not_rollback() {
        // last_good absent (corrupt status) + running == candidate + crashloop: the
        // old `last_good.or(running)` fallback would "roll back" onto the crashing
        // candidate. Now it fails CLOSED to terminal Failed — never a RollingBack
        // toward the candidate, never marks it good.
        let u = rolling_status();
        let mut o = obs("img:v2", Some("img:v2"), None, false, Some(&u));
        o.rolling_crashloop = true;
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::Failed),
            "no baseline ⇒ terminal Failed, NOT a RollingBack targeting the candidate"
        );
        assert_eq!(p.mark_last_good, None, "the candidate is never marked good");
    }

    #[test]
    fn rollingback_with_no_baseline_halts_terminal_not_onto_candidate() {
        // last_good absent + running == candidate + healthy: the old code would
        // "complete" the rollback onto the candidate and mark it. Now it fails
        // closed to terminal Failed.
        let u = rollingback_status();
        let o = obs("img:v2", Some("img:v2"), None, true, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(p.next_upgrade.unwrap().phase, Some(UpgradePhase::Failed));
        assert_eq!(p.mark_last_good, None);
    }

    // ---------- unparseable deadline fails CLOSED (LOW-6) ----------

    #[test]
    fn unparseable_rolling_deadline_is_treated_as_expired() {
        // A corrupt/empty deadline must fail CLOSED (roll back), not run unbounded.
        let mut u = rolling_status();
        u.deadline_at = "not-a-timestamp".to_string();
        let o = obs("img:v2", Some("img:v2"), Some("img:v1"), false, Some(&u));
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "unparseable deadline ⇒ expired ⇒ rollback to last-good"
        );
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
    }

    #[test]
    fn unparseable_preflight_deadline_fails_closed() {
        let mut u = preflighting_status();
        u.deadline_at = String::new(); // empty ⇒ unparseable ⇒ expired
        let mut o = obs("img:v2", Some("img:v1"), Some("img:v1"), true, Some(&u));
        o.preflight = BootOutcome::Booting;
        let p = plan(&o, &cfg(true));
        assert_eq!(
            p.effective_image, "img:v1",
            "never flips — production stays on the current image"
        );
        assert_eq!(
            p.next_upgrade.unwrap().phase,
            Some(UpgradePhase::Failed),
            "an unparseable pre-flight deadline fails closed"
        );
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
            preflight_netpol_name,
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
    fn sequence_soak_catches_a_late_crash_and_keeps_prod_on_last_good() {
        // The MED-3 lifecycle: the candidate rolls out healthy, begins its soak,
        // then crash-loops under load DURING the soak. It is auto-rolled-back and
        // last-good is NEVER advanced to it (no poisoning).
        let cfg = cfg_soak(true, 60);
        let t0 = ts("2026-07-14T12:00:00Z");
        let mut w = World::fresh();
        w.step("img:v1", false, BootOutcome::Booting, false, t0, &cfg);
        w.step("img:v1", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(w.last_good.as_deref(), Some("img:v1"));
        // Upgrade to v2: pre-flight passes, flip to v2.
        w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg); // start pre-flight
        let p = w.step("img:v2", false, BootOutcome::Ready, false, t0, &cfg); // flip → Rolling
        assert_eq!(p.effective_image, "img:v2");
        // v2 healthy → soak begins; NOT yet committed.
        let p = w.step("img:v2", true, BootOutcome::Booting, false, t0, &cfg);
        assert_eq!(p.effective_image, "img:v2");
        assert!(
            w.upgrade.is_some(),
            "still Rolling — soaking, not yet Succeeded"
        );
        assert_eq!(
            w.last_good.as_deref(),
            Some("img:v1"),
            "last-good NOT advanced mid-soak"
        );
        // 30s later, still healthy but the 60s soak has not elapsed.
        let t30 = ts("2026-07-14T12:00:30Z");
        w.step("img:v2", true, BootOutcome::Booting, false, t30, &cfg);
        assert!(w.upgrade.is_some(), "still soaking at 30s < 60s");
        assert_eq!(w.last_good.as_deref(), Some("img:v1"));
        // At 40s the candidate crash-loops under load → auto-rollback.
        let t40 = ts("2026-07-14T12:00:40Z");
        let p = w.step("img:v2", false, BootOutcome::Booting, true, t40, &cfg);
        assert_eq!(
            p.effective_image, "img:v1",
            "crash during soak ⇒ revert to last-good"
        );
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::RollingBack)
        );
        // Rollback completes; last-good never became v2.
        let p = w.step("img:v2", true, BootOutcome::Booting, false, t40, &cfg);
        assert_eq!(p.effective_image, "img:v1");
        assert_eq!(
            w.upgrade.as_ref().unwrap().phase,
            Some(UpgradePhase::Failed)
        );
        assert_eq!(
            w.last_good.as_deref(),
            Some("img:v1"),
            "INVARIANT: a briefly-healthy candidate NEVER poisons last-good"
        );
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

    // ---------- HIGH-1: deny-all-egress NetworkPolicy on the pre-flight pod ----------

    #[test]
    fn preflight_netpol_denies_all_egress_and_selects_only_the_preflight_pod() {
        // The candidate boots the real image with the real master key + SA — the
        // clone isolates DATA, this policy isolates the NETWORK so no live side
        // effect (external-DB migration, S3 push, KMS write, IAM register,
        // notification/billing/webhook) can execute during the pre-flight.
        let mut i = stateful_inputs();
        // Even when the pod shares the app's `app.kubernetes.io/name`, the policy
        // must NOT select on it (that would deny production egress).
        i.labels
            .insert("app.kubernetes.io/name".to_string(), "cloud".to_string());
        let np = build_preflight_netpol("cloud", "img:v2", &i);
        let spec = np.spec.unwrap();

        // Egress-only isolation with an EMPTY egress rule set ⇒ deny ALL egress.
        assert_eq!(
            spec.policy_types.as_deref(),
            Some(&["Egress".to_string()][..])
        );
        assert_eq!(
            spec.egress.as_ref().map(|e| e.len()),
            Some(0),
            "empty egress rule set ⇒ deny all egress"
        );

        // Selects ONLY the pre-flight pod (its two unique pre-flight labels),
        // never a production pod that shares the app label.
        let sel = spec.pod_selector.unwrap().match_labels.unwrap();
        assert_eq!(
            sel.get(PREFLIGHT_OF_LABEL).map(String::as_str),
            Some("cloud")
        );
        assert_eq!(
            sel.get(PREFLIGHT_TARGET_LABEL).map(String::as_str),
            Some(target_hash("img:v2").as_str())
        );
        assert!(
            !sel.contains_key("app.kubernetes.io/name"),
            "must NOT select on the shared app label (would deny production egress)"
        );

        // Owned + labelled like the pod so `converge_preflight` sweeps it the same.
        assert!(np.metadata.owner_references.is_some());
        let labels = np.metadata.labels.unwrap();
        assert_eq!(
            labels.get(PREFLIGHT_OF_LABEL).map(String::as_str),
            Some("cloud")
        );
        assert_eq!(
            labels.get(PREFLIGHT_TARGET_LABEL).map(String::as_str),
            Some(target_hash("img:v2").as_str())
        );
    }

    // ---------- HIGH-1: pre-flight refusal guard ----------

    #[test]
    fn upgrade_refusal_requires_boot_env_for_a_stateful_upgrade() {
        assert!(
            upgrade_refusal(true, true, false).is_some(),
            "stateful + no bootEnv ⇒ refused"
        );
        assert!(
            upgrade_refusal(true, false, false).is_none(),
            "stateful + bootEnv ⇒ allowed"
        );
        assert!(
            upgrade_refusal(false, true, false).is_none(),
            "stateless + no bootEnv ⇒ allowed (boot-only, no live data to protect)"
        );
    }

    #[test]
    fn upgrade_refusal_rejects_a_declared_live_app_db_volume() {
        // A hand-declared live app-db PVC is refused regardless of stateful/bootEnv
        // — it would smuggle the live volume into the pre-flight, defeating the
        // clone-not-live isolation. Checked first (highest precedence).
        assert!(upgrade_refusal(true, false, true).is_some());
        assert!(upgrade_refusal(false, false, true).is_some());
        assert!(upgrade_refusal(false, true, true).is_some());
    }
}
