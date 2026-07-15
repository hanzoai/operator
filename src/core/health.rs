//! Pure health predicates over live `Deployment` / `Pod` objects.
//!
//! ONE home for "is this workload healthy?" so the Service upgrade FSM
//! (`controllers::upgrade`) and the apps DRIVE controller (`controllers::apps`)
//! read the SAME rollout signal instead of each re-deriving it. Every function
//! here is pure over its k8s-openapi input, so the FSM's health gate is
//! unit-tested with hand-built objects — no cluster.

use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::core::v1::Pod;

/// A container that has restarted this many times is treated as crashlooping
/// even before the kubelet stamps `CrashLoopBackOff` — the fast-rollback
/// signal, so a bad candidate does not burn the full rollout deadline in
/// downtime.
pub const CRASH_RESTART_THRESHOLD: i32 = 3;

/// Waiting reasons that mean a container will never become ready without a spec
/// change — a crash loop or an un-pullable / un-createable image. Observing any
/// of these on a rolling pod is grounds for immediate auto-rollback.
const TERMINAL_WAITING_REASONS: &[&str] = &[
    "CrashLoopBackOff",
    "ImagePullBackOff",
    "ErrImagePull",
    "InvalidImageName",
    "CreateContainerConfigError",
    "CreateContainerError",
];

/// True when the named Deployment reports a complete rollout — the same
/// condition `kubectl rollout status` waits on: the controller has observed the
/// latest spec generation, every desired replica is updated and available, and
/// no old replicas linger.
///
/// Lifted verbatim from the apps DRIVE controller so both call sites share one
/// definition (the apps controller now calls this).
pub fn rollout_complete(dep: &Deployment) -> bool {
    let generation = dep.metadata.generation;
    let spec_replicas = dep.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let status = match &dep.status {
        Some(s) => s,
        None => return false,
    };
    // The controller must have acted on the current spec.
    if let (Some(gen), Some(observed)) = (generation, status.observed_generation) {
        if observed < gen {
            return false;
        }
    }
    let updated = status.updated_replicas.unwrap_or(0);
    let available = status.available_replicas.unwrap_or(0);
    let total = status.replicas.unwrap_or(0);
    // Every desired replica updated to the new template…
    updated >= spec_replicas
        // …no old replicas still around (total not exceeding desired)…
        && total <= updated
        // …and all desired replicas available.
        && available >= spec_replicas
}

/// True when the Deployment is fully healthy: its rollout is complete AND it
/// runs at least one replica. This is the FSM's "production is healthy on the
/// currently-deployed image" signal — an upgrade is judged Succeeded, and a
/// rollback judged complete, only when this holds.
pub fn deployment_healthy(dep: &Deployment) -> bool {
    let spec_replicas = dep.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    spec_replicas > 0 && rollout_complete(dep)
}

/// The main-container image a live Deployment currently runs (container[0]).
/// `None` when the Deployment or its pod template carries no container.
pub fn deployment_image(dep: &Deployment) -> Option<String> {
    dep.spec
        .as_ref()
        .and_then(|s| s.template.spec.as_ref())
        .and_then(|ps| ps.containers.first())
        .and_then(|c| c.image.clone())
}

/// Outcome of observing a pre-flight candidate pod (restartPolicy: Never).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootOutcome {
    /// The candidate reached readiness — it booted over the (cloned) data.
    Ready,
    /// The candidate crashed / could not be pulled — the pre-flight FAILS and
    /// production is never flipped.
    Failed,
    /// Still starting; no terminal signal yet (the FSM's deadline bounds this).
    Booting,
}

/// Classify a pre-flight candidate pod. A pre-flight pod runs `restartPolicy:
/// Never`, so a migrate/boot crash terminates a container non-zero (and stays
/// terminated for us to observe) rather than looping. `Ready` is the pod's
/// `Ready` condition — for a server image that is the readiness probe passing,
/// i.e. the candidate got all the way through migrate + mount + bind.
pub fn pod_boot_outcome(pod: &Pod) -> BootOutcome {
    let status = match &pod.status {
        Some(s) => s,
        None => return BootOutcome::Booting,
    };
    // Terminal pod phase.
    if status.phase.as_deref() == Some("Failed") {
        return BootOutcome::Failed;
    }
    // Any container terminated non-zero, or waiting in a terminal reason ⇒
    // the candidate cannot come up over this data.
    if let Some(cs) = &status.container_statuses {
        for c in cs {
            if let Some(state) = &c.state {
                if let Some(term) = &state.terminated {
                    if term.exit_code != 0 {
                        return BootOutcome::Failed;
                    }
                }
                if let Some(waiting) = &state.waiting {
                    if let Some(reason) = waiting.reason.as_deref() {
                        if TERMINAL_WAITING_REASONS.contains(&reason) {
                            return BootOutcome::Failed;
                        }
                    }
                }
            }
        }
    }
    // Ready condition True ⇒ booted successfully.
    if pod_ready(pod) {
        return BootOutcome::Ready;
    }
    BootOutcome::Booting
}

/// True when the pod's `Ready` condition is `True`.
pub fn pod_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conds| {
            conds
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
        .unwrap_or(false)
}

/// True when a pod carries a container that is crash-looping or has an
/// un-recoverable image error — the fast-rollback signal used during a
/// health-gated roll (before the deadline expires).
pub fn pod_crashlooping(pod: &Pod) -> bool {
    let Some(status) = &pod.status else {
        return false;
    };
    let Some(container_statuses) = &status.container_statuses else {
        return false;
    };
    for c in container_statuses {
        if c.restart_count >= CRASH_RESTART_THRESHOLD {
            return true;
        }
        if let Some(state) = &c.state {
            if let Some(waiting) = &state.waiting {
                if let Some(reason) = waiting.reason.as_deref() {
                    if TERMINAL_WAITING_REASONS.contains(&reason) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// True when ANY pod in `pods` is crash-looping. The caller pre-filters to the
/// pods running the candidate image so a crashing OLD replica (already being
/// torn down) never trips the rollback.
pub fn any_pod_crashlooping(pods: &[Pod]) -> bool {
    pods.iter().any(pod_crashlooping)
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStatus};
    use k8s_openapi::api::core::v1::{
        Container, ContainerState, ContainerStateTerminated, ContainerStateWaiting,
        ContainerStatus, Pod, PodCondition, PodSpec, PodStatus, PodTemplateSpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn dep(generation: i64, replicas: i32, st: DeploymentStatus) -> Deployment {
        Deployment {
            metadata: ObjectMeta {
                generation: Some(generation),
                ..Default::default()
            },
            spec: Some(DeploymentSpec {
                replicas: Some(replicas),
                ..Default::default()
            }),
            status: Some(st),
        }
    }

    #[test]
    fn rollout_complete_true_when_converged() {
        let d = dep(
            4,
            2,
            DeploymentStatus {
                observed_generation: Some(4),
                updated_replicas: Some(2),
                available_replicas: Some(2),
                replicas: Some(2),
                ..Default::default()
            },
        );
        assert!(rollout_complete(&d));
        assert!(deployment_healthy(&d));
    }

    #[test]
    fn rollout_incomplete_when_observed_generation_behind() {
        let d = dep(
            5,
            2,
            DeploymentStatus {
                observed_generation: Some(4),
                updated_replicas: Some(2),
                available_replicas: Some(2),
                replicas: Some(2),
                ..Default::default()
            },
        );
        assert!(!rollout_complete(&d));
        assert!(!deployment_healthy(&d));
    }

    #[test]
    fn rollout_incomplete_when_old_replicas_linger() {
        let d = dep(
            4,
            2,
            DeploymentStatus {
                observed_generation: Some(4),
                updated_replicas: Some(2),
                available_replicas: Some(2),
                replicas: Some(3), // an old replica still around
                ..Default::default()
            },
        );
        assert!(!rollout_complete(&d));
    }

    #[test]
    fn rollout_incomplete_when_no_status() {
        let mut d = dep(1, 1, DeploymentStatus::default());
        d.status = None;
        assert!(!rollout_complete(&d));
        assert!(!deployment_healthy(&d));
    }

    #[test]
    fn deployment_unhealthy_when_scaled_to_zero() {
        // A 0-replica Deployment that "rolled out" is not a healthy prod signal.
        let d = dep(
            1,
            0,
            DeploymentStatus {
                observed_generation: Some(1),
                updated_replicas: Some(0),
                available_replicas: Some(0),
                replicas: Some(0),
                ..Default::default()
            },
        );
        assert!(!deployment_healthy(&d));
    }

    #[test]
    fn deployment_image_reads_first_container() {
        let mut d = dep(1, 1, DeploymentStatus::default());
        d.spec.as_mut().unwrap().template = PodTemplateSpec {
            spec: Some(PodSpec {
                containers: vec![Container {
                    name: "main".into(),
                    image: Some("ghcr.io/hanzoai/cloud:v1.0.0".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            deployment_image(&d).as_deref(),
            Some("ghcr.io/hanzoai/cloud:v1.0.0")
        );
    }

    fn pod_with(phase: &str, statuses: Vec<ContainerStatus>, ready: bool) -> Pod {
        Pod {
            status: Some(PodStatus {
                phase: Some(phase.into()),
                container_statuses: Some(statuses),
                conditions: Some(vec![PodCondition {
                    type_: "Ready".into(),
                    status: if ready { "True".into() } else { "False".into() },
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn waiting(reason: &str) -> ContainerStatus {
        ContainerStatus {
            name: "main".into(),
            restart_count: 0,
            state: Some(ContainerState {
                waiting: Some(ContainerStateWaiting {
                    reason: Some(reason.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn terminated(exit: i32) -> ContainerStatus {
        ContainerStatus {
            name: "main".into(),
            restart_count: 0,
            state: Some(ContainerState {
                terminated: Some(ContainerStateTerminated {
                    exit_code: exit,
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn boot_outcome_ready_when_ready_condition_true() {
        let p = pod_with("Running", vec![], true);
        assert_eq!(pod_boot_outcome(&p), BootOutcome::Ready);
    }

    #[test]
    fn boot_outcome_failed_on_nonzero_exit() {
        // A migration crash: the candidate boots, migrate() dies, container
        // exits 1. restartPolicy Never keeps it terminated for us to observe.
        let p = pod_with("Running", vec![terminated(1)], false);
        assert_eq!(pod_boot_outcome(&p), BootOutcome::Failed);
    }

    #[test]
    fn boot_outcome_failed_on_pod_failed_phase() {
        let p = pod_with("Failed", vec![], false);
        assert_eq!(pod_boot_outcome(&p), BootOutcome::Failed);
    }

    #[test]
    fn boot_outcome_failed_on_image_pull_error() {
        let p = pod_with("Pending", vec![waiting("ImagePullBackOff")], false);
        assert_eq!(pod_boot_outcome(&p), BootOutcome::Failed);
    }

    #[test]
    fn boot_outcome_booting_while_starting() {
        let p = pod_with("Pending", vec![waiting("ContainerCreating")], false);
        assert_eq!(pod_boot_outcome(&p), BootOutcome::Booting);
    }

    #[test]
    fn boot_outcome_zero_exit_is_not_failed() {
        // A boot-check that exits 0 is a clean success signal, not a failure.
        let p = pod_with("Succeeded", vec![terminated(0)], false);
        assert_ne!(pod_boot_outcome(&p), BootOutcome::Failed);
    }

    #[test]
    fn crashlooping_on_backoff_reason() {
        let p = pod_with("Running", vec![waiting("CrashLoopBackOff")], false);
        assert!(pod_crashlooping(&p));
        assert!(any_pod_crashlooping(std::slice::from_ref(&p)));
    }

    #[test]
    fn crashlooping_on_high_restart_count() {
        let mut cs = terminated(1);
        cs.restart_count = CRASH_RESTART_THRESHOLD;
        let p = pod_with("Running", vec![cs], false);
        assert!(pod_crashlooping(&p));
    }

    #[test]
    fn not_crashlooping_when_healthy() {
        let p = pod_with("Running", vec![], true);
        assert!(!pod_crashlooping(&p));
        assert!(!any_pod_crashlooping(&[p]));
    }
}
