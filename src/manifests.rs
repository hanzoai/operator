//! K8s object builders.
//!
//! Parallel to the Go `internal/manifests/` package. Pure functions that
//! return canonical `k8s_openapi` types — no I/O, no controller dispatch.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{
    Deployment, DeploymentSpec, DeploymentStrategy, RollingUpdateDeployment, StatefulSet,
    StatefulSetSpec, StatefulSetUpdateStrategy,
};
use k8s_openapi::api::autoscaling::v2::{
    CrossVersionObjectReference, HorizontalPodAutoscaler, HorizontalPodAutoscalerSpec, MetricSpec,
    MetricTarget, ResourceMetricSource,
};
use k8s_openapi::api::core::v1::{
    Affinity, ConfigMap, Container, ContainerPort, EnvFromSource, EnvVar, ExecAction,
    HTTPGetAction, Lifecycle, LifecycleHandler, LocalObjectReference, PersistentVolumeClaim,
    PodAffinity, PodAffinityTerm, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
    ResourceRequirements as K8sResourceRequirements, Service as CoreService, ServicePort,
    ServiceSpec as CoreServiceSpec, TCPSocketAction, Toleration, TopologySpreadConstraint, Volume,
    VolumeMount, WeightedPodAffinityTerm,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec as K8sIngressSpec, IngressTLS, NetworkPolicy,
    NetworkPolicyIngressRule, NetworkPolicyPeer as K8sNetworkPolicyPeer,
    NetworkPolicySpec as K8sNetworkPolicySpec, ServiceBackendPort,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec as K8sPDBSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::crd::{
    AutoscalingSpec, IngressSpec, NetworkPolicySpec, PodDisruptionBudgetSpec, ProbeSpec,
    ResourceRequirements, ServicePort as CrServicePort, DEFAULT_INGRESS_CLASS,
};

pub const LABEL_NAME: &str = "app.kubernetes.io/name";
pub const LABEL_INSTANCE: &str = "app.kubernetes.io/instance";
pub const LABEL_COMPONENT: &str = "app.kubernetes.io/component";
pub const LABEL_PART_OF: &str = "app.kubernetes.io/part-of";
pub const LABEL_VERSION: &str = "app.kubernetes.io/version";
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY_VALUE: &str = "hanzo-operator";

/// Coerce an image tag / ref into a valid Kubernetes label value for
/// `app.kubernetes.io/version`. A label value must be ≤63 chars, contain only
/// `[A-Za-z0-9._-]`, and start + end alphanumeric.
///
/// Digest-pinned refs are now the canonical deploy pattern (universe#445), so
/// `spec.image.tag` can carry `v8.4.118@sha256:9820e153…`. That value blows
/// BOTH the 63-char limit and the charset (`@`, `:` are illegal), so inserting
/// it verbatim made the API server reject the whole Deployment
/// (`metadata.labels: Invalid value`) — the `console` reconcile storm.
///
/// Rule: keep the human tag before any `@` digest, replace remaining illegal
/// chars with `-`, cap at 63, and trim back to an alphanumeric boundary. A
/// bare-digest ref (nothing before `@`) folds to a valid `sha256-<hex…>`.
/// Deterministic: same ref → same value (no rollout churn).
pub fn sanitize_label_value(v: &str) -> String {
    // Prefer the human tag before a digest; fall back to the whole ref.
    let base = match v.split_once('@') {
        Some((tag, _digest)) if !tag.is_empty() => tag,
        _ => v,
    };
    // Replace any char outside the label alphabet. ASCII-only after this, so a
    // subsequent byte-truncate can't split a char.
    let mapped: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let capped = &mapped[..mapped.len().min(63)];
    // Must start AND end with an alphanumeric.
    capped
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_string()
}

/// The single non-selector label EVERY operator-managed object carries:
/// `app.kubernetes.io/managed-by = hanzo-operator`. It attributes the creator
/// but is NOT a key any operator Service selects on (Services select on
/// [`selector_labels`] = name+instance ONLY), so a pod carrying just this label
/// can never be re-added to a production endpoint set nor re-selected by an
/// app-labelled egress-allow. It is therefore the MINIMAL functional base for a
/// pre-flight resource: the pre-flight identity comes from the two
/// `hanzo.ai/preflight-*` keys, and `managed-by` is the only descriptive key that
/// stays.
pub fn managed_by_labels() -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_MANAGED_BY.to_string(), MANAGED_BY_VALUE.to_string());
    labels
}

/// The DESCRIPTIVE `app.kubernetes.io/*` labels: `managed-by` (+ `component`,
/// `part-of`, `version` when non-empty). Deliberately EXCLUDES the two selector
/// keys (`name`, `instance`) — these labels describe a workload but never select
/// it. The `version` label is sanitized via [`sanitize_label_value`].
///
/// NOTE: a pre-flight resource does NOT use this full set — `component`,
/// `part-of`, `version` are SHARED with production pods and would widen the
/// selector surface a pre-flight pod exposes (an egress-allow or headless Service
/// selecting `part-of` could re-reach the unproven candidate). The pre-flight
/// carries only [`managed_by_labels`]; this richer set is for real workloads.
pub fn descriptive_labels(
    component: &str,
    part_of: &str,
    version: &str,
) -> BTreeMap<String, String> {
    let mut labels = managed_by_labels();
    if !component.is_empty() {
        labels.insert(LABEL_COMPONENT.to_string(), component.to_string());
    }
    if !part_of.is_empty() {
        labels.insert(LABEL_PART_OF.to_string(), part_of.to_string());
    }
    // Sanitize: a digest-pinned `version` (repo tag `vX.Y.Z@sha256:…`) is not a
    // valid label value and would fail the Deployment apply.
    let version = sanitize_label_value(version);
    if !version.is_empty() {
        labels.insert(LABEL_VERSION.to_string(), version);
    }
    labels
}

/// Build the standard `app.kubernetes.io/*` label set: the selector keys
/// ([`selector_labels`]) UNION the descriptive keys ([`descriptive_labels`]).
pub fn standard_labels(
    name: &str,
    component: &str,
    part_of: &str,
    version: &str,
) -> BTreeMap<String, String> {
    let mut labels = selector_labels(name);
    labels.extend(descriptive_labels(component, part_of, version));
    labels
}

/// Minimal label set for pod selectors. Must be immutable after creation.
pub fn selector_labels(name: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    out.insert(LABEL_NAME.to_string(), name.to_string());
    out.insert(LABEL_INSTANCE.to_string(), name.to_string());
    out
}

/// Merge label maps in order; later entries override earlier on key collision.
pub fn merge_labels(maps: &[&BTreeMap<String, String>]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for m in maps {
        for (k, v) in m.iter() {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Inject a preStop sleep on every container that lacks one. Gives pods 5
/// seconds to drain before SIGTERM.
fn inject_pre_stop(containers: Vec<Container>) -> Vec<Container> {
    containers
        .into_iter()
        .map(|mut c| {
            let lifecycle = c.lifecycle.get_or_insert_with(Lifecycle::default);
            if lifecycle.pre_stop.is_none() {
                lifecycle.pre_stop = Some(LifecycleHandler {
                    exec: Some(ExecAction {
                        command: Some(vec![
                            "/bin/sh".to_string(),
                            "-c".to_string(),
                            "sleep 5".to_string(),
                        ]),
                    }),
                    ..Default::default()
                });
            }
            c
        })
        .collect()
}

/// Convert operator ResourceRequirements to k8s ResourceRequirements.
/// Maps `String` quantities → k8s `Quantity` (String is wire-compatible).
pub fn to_k8s_resources(spec: &ResourceRequirements) -> K8sResourceRequirements {
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    let to_q = |m: &BTreeMap<String, String>| {
        m.iter()
            .map(|(k, v)| (k.clone(), Quantity(v.clone())))
            .collect::<BTreeMap<_, _>>()
    };
    K8sResourceRequirements {
        requests: spec.requests.as_ref().map(to_q),
        limits: spec.limits.as_ref().map(to_q),
        ..Default::default()
    }
}

/// Build an HTTP GET probe.
pub fn build_http_probe(spec: &ProbeSpec) -> Probe {
    Probe {
        http_get: Some(HTTPGetAction {
            path: if spec.path.is_empty() {
                Some("/health".to_string())
            } else {
                Some(spec.path.clone())
            },
            port: IntOrString::Int(spec.port),
            ..Default::default()
        }),
        initial_delay_seconds: if spec.initial_delay_seconds > 0 {
            Some(spec.initial_delay_seconds)
        } else {
            Some(5)
        },
        period_seconds: if spec.period_seconds > 0 {
            Some(spec.period_seconds)
        } else {
            Some(10)
        },
        ..Default::default()
    }
}

/// Build a k8s `Probe` from a CR `ProbeSpec`, dispatching to the declared
/// handler. Returns `None` when the spec declares no usable handler, so the
/// caller emits NO probe rather than an invalid one.
///
/// Handler precedence: `exec` → `tcpSocket` → `httpGet` (`port > 0`).
///
/// This is the fix for the reconcile storm where non-HTTP datastores
/// (`insights-sql` `pg_isready`, `insights-kv` `redis-cli ping`,
/// `insights-kafka` TCP `9092`) declared `exec`/`tcpSocket` probes that the
/// old HTTP-only `ProbeSpec` dropped — leaving `port: 0` and emitting an
/// `httpGet` the API server rejected (`port: Invalid value: 0: must be between
/// 1 and 65535`). We now honor the real handler and NEVER emit a port-0
/// `httpGet`.
pub fn build_probe(spec: &ProbeSpec) -> Option<Probe> {
    let timing = |mut p: Probe| -> Probe {
        p.initial_delay_seconds = Some(if spec.initial_delay_seconds > 0 {
            spec.initial_delay_seconds
        } else {
            5
        });
        p.period_seconds = Some(if spec.period_seconds > 0 {
            spec.period_seconds
        } else {
            10
        });
        // Same lenient floor `default_readiness_probe` applies. Without it a CR
        // that DECLARES a probe got a STRICTER budget (k8s defaults: 1s timeout,
        // 3 failures) than one that declared none (3s, 6) — backwards, and the
        // cause of spurious NotReady/restarts on any service that can block
        // longer than a second. A single-writer store under load is the normal
        // case, not the exception: hanzo-git liveness-timed-out on a healthy pod
        // mid-mirror-sync. `ProbeSpec` models neither field, so there is nothing
        // to override and this floor is unconditional.
        p.timeout_seconds = Some(3);
        p.failure_threshold = Some(6);
        p
    };
    if let Some(e) = &spec.exec {
        if !e.command.is_empty() {
            return Some(timing(Probe {
                exec: Some(ExecAction {
                    command: Some(e.command.clone()),
                }),
                ..Default::default()
            }));
        }
    }
    if let Some(t) = &spec.tcp_socket {
        if t.port > 0 {
            return Some(timing(Probe {
                tcp_socket: Some(TCPSocketAction {
                    port: IntOrString::Int(t.port),
                    ..Default::default()
                }),
                ..Default::default()
            }));
        }
    }
    if spec.port > 0 {
        // Reuse the HTTP builder for the handler + path, but route it through
        // `timing` like every other handler so ONE place owns probe timing.
        return Some(timing(build_http_probe(spec)));
    }
    None
}

/// Default readiness probe for an App whose CR OMITS `readinessProbe`.
///
/// `maxUnavailable=0` (the rollout default in `build_deployment`) only gates a
/// roll when k8s can tell the NEW pod is unhealthy. With NO readiness probe,
/// k8s marks a broken-but-running pod `Ready` the instant its process starts,
/// so `maxUnavailable=0` gives zero protection — a broken image rolls right
/// over the healthy pod. Defaulting a probe here is what makes that outage
/// class impossible.
///
/// The default is a TCP-socket "is the first port accepting connections?"
/// probe, deliberately NOT an HTTP `/health` GET: an HTTP default would 404 on
/// every service that does not implement that path, so a GOOD image would never
/// become `Ready` and its roll would stall fleet-wide. A TCP-socket probe
/// cannot false-negative a healthy service that binds its port, yet still
/// catches a broken image that fails to listen — strictly better than no probe.
///
/// Returns `None` for a port-less workload (a queue worker with no listener has
/// nothing to TCP-probe) so its Deployment stays byte-identical. Services that
/// want real HTTP health-checking declare an explicit `readinessProbe` in their
/// CR; those are honored as-is and never reach this default.
///
/// Timing is a deliberately-lenient fleet-wide floor: `initialDelay 10 +
/// failureThreshold 6 × period 10 ≈ 70s` of not-listening before a pod fails
/// readiness. Wide enough to cover on-boot-migration / model-load / JIT-warmup
/// starters without stalling a GOOD roll, yet still catches a truly-broken image
/// within ~a minute (its roll stalls under `maxUnavailable=0` while the healthy
/// old pod keeps serving).
pub fn default_readiness_probe(ports: &[CrServicePort]) -> Option<Probe> {
    let first = ports.first()?;
    Some(Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(first.container_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(10),
        period_seconds: Some(10),
        timeout_seconds: Some(3),
        failure_threshold: Some(6),
        success_threshold: Some(1),
        ..Default::default()
    })
}

/// Convert CR ServicePorts to k8s ContainerPorts.
pub fn container_ports(ports: &[CrServicePort]) -> Vec<ContainerPort> {
    ports
        .iter()
        .map(|p| ContainerPort {
            name: Some(p.name.clone()),
            container_port: p.container_port,
            protocol: if p.protocol.is_empty() {
                None
            } else {
                Some(p.protocol.clone())
            },
            ..Default::default()
        })
        .collect()
}

/// Convert CR ServicePorts to k8s Service ports.
pub fn service_ports(ports: &[CrServicePort]) -> Vec<ServicePort> {
    ports
        .iter()
        .map(|p| ServicePort {
            name: Some(p.name.clone()),
            port: p.service_port.unwrap_or(p.container_port),
            target_port: Some(IntOrString::Int(p.container_port)),
            protocol: if p.protocol.is_empty() {
                None
            } else {
                Some(p.protocol.clone())
            },
            ..Default::default()
        })
        .collect()
}

/// Build a Deployment with standard rolling-update settings.
#[allow(clippy::too_many_arguments)]
/// WHERE a workload runs, as ONE value.
///
/// Grouped rather than splayed into three more positional parameters because
/// placement is a single concern: a dedicated pool is `nodeSelector` +
/// `tolerations` + a preempting `priorityClassName` acting TOGETHER, and any one
/// of them alone is a partial answer. `Placement::default()` means "no opinion",
/// which renders every field `None` — a byte-identical PodSpec.
///
/// This is the abstraction whose absence made `crs/cloud.yaml` express placement
/// through `resources.requests`. Resources say what a workload NEEDS; this says
/// where it GOES. Keeping them separate is the whole point.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Placement {
    pub node_selector: Option<BTreeMap<String, String>>,
    pub tolerations: Vec<Toleration>,
    pub priority_class_name: String,
}

impl Placement {
    /// True when the caller expressed no placement opinion at all — the state in
    /// which this feature must be invisible.
    pub fn is_empty(&self) -> bool {
        self.node_selector.as_ref().is_none_or(|m| m.is_empty())
            && self.tolerations.is_empty()
            && self.priority_class_name.is_empty()
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_deployment(
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    selector_labels_map: BTreeMap<String, String>,
    replicas: Option<i32>,
    containers: Vec<Container>,
    volumes: Vec<Volume>,
    strategy: &str,
    image_pull_secrets: Vec<LocalObjectReference>,
    service_account_name: &str,
    placement: Placement,
) -> Deployment {
    let s = if strategy == "Recreate" {
        DeploymentStrategy {
            type_: Some("Recreate".to_string()),
            ..Default::default()
        }
    } else {
        DeploymentStrategy {
            type_: Some("RollingUpdate".to_string()),
            rolling_update: Some(RollingUpdateDeployment {
                max_surge: Some(IntOrString::Int(1)),
                max_unavailable: Some(IntOrString::Int(0)),
            }),
        }
    };

    let containers = inject_pre_stop(containers);

    // Best-effort node spread so a multi-replica app is REAL HA — two replicas on
    // one node die together on node loss. Computed before `selector_labels_map` is
    // moved into the Deployment selector below.
    let topology_spread = default_topology_spread(replicas, &selector_labels_map);

    Deployment {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas,
            min_ready_seconds: Some(10),
            selector: LabelSelector {
                match_labels: Some(selector_labels_map),
                ..Default::default()
            },
            strategy: Some(s),
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers,
                    volumes: if volumes.is_empty() {
                        None
                    } else {
                        Some(volumes)
                    },
                    image_pull_secrets: if image_pull_secrets.is_empty() {
                        None
                    } else {
                        Some(image_pull_secrets)
                    },
                    service_account_name: if service_account_name.is_empty() {
                        None
                    } else {
                        Some(service_account_name.to_string())
                    },
                    topology_spread_constraints: topology_spread,
                    // Placement. Each renders `None` when the CR said nothing,
                    // so an omitting App is byte-identical to one built before
                    // these fields existed.
                    node_selector: placement.node_selector.filter(|m| !m.is_empty()),
                    tolerations: if placement.tolerations.is_empty() {
                        None
                    } else {
                        Some(placement.tolerations)
                    },
                    priority_class_name: if placement.priority_class_name.is_empty() {
                        None
                    } else {
                        Some(placement.priority_class_name)
                    },
                    termination_grace_period_seconds: Some(30),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Best-effort node spread for a multi-replica Deployment, so its replicas don't
/// all land on one node and die together when that node is lost — the difference
/// between nominal `replicas: 2` and REAL high availability.
///
/// `maxSkew: 1` over `kubernetes.io/hostname`, keyed on the app's OWN selector, so
/// each app spreads only against itself. `whenUnsatisfiable: ScheduleAnyway` makes
/// it SOFT: on a constrained cluster (fewer schedulable nodes than replicas, a
/// drain, a taint) the pod still schedules rather than going Pending — availability
/// is never traded for spread. Returns `None` for a singleton (`replicas <= 1`):
/// nothing to spread, and an unset field keeps those Deployments byte-identical
/// (incl. `surgeColocation` singletons, which are `replicas: 1`).
pub fn default_topology_spread(
    replicas: Option<i32>,
    selector_labels_map: &BTreeMap<String, String>,
) -> Option<Vec<TopologySpreadConstraint>> {
    if replicas.unwrap_or(1) <= 1 {
        return None;
    }
    Some(vec![TopologySpreadConstraint {
        max_skew: 1,
        topology_key: "kubernetes.io/hostname".to_string(),
        when_unsatisfiable: "ScheduleAnyway".to_string(),
        label_selector: Some(LabelSelector {
            match_labels: Some(selector_labels_map.clone()),
            ..Default::default()
        }),
        ..Default::default()
    }])
}

/// Soft self-podAffinity that co-locates a rolling surge pod on the SAME node
/// as the app's already-running pods (topologyKey hostname, matching the app's
/// own selector). For a service whose data lives on a single ReadWriteOnce PVC,
/// this lets the surge pod bind-mount the already-attached volume — DO block
/// storage is single-attach, so RWO permits multiple pods per NODE but not a
/// second node — instead of dead-locking on a "Multi-Attach" error. That turns
/// a RollingUpdate over the volume into a zero-downtime, same-host handoff.
///
/// PREFERRED, never required: with no anchor pod (cold start / node loss) the
/// surge still schedules anywhere and recovers; a rare failure to co-locate is a
/// fail-SAFE stalled roll (old pod keeps serving under maxUnavailable=0), never
/// an outage or a cross-node split-brain writer.
///
/// OPT-IN by the caller (`ServiceSpec.surgeColocation`): the brief same-node
/// two-pod overlap is only safe for stores that tolerate concurrent same-host
/// opens (SQLite in WAL mode + `busy_timeout`). An exclusive-lock single-open
/// engine (Badger, LMDB, Qdrant, …) must stay on strategy `Recreate` instead,
/// so this is never applied automatically.
pub fn colocation_affinity(selector_labels_map: &BTreeMap<String, String>) -> Affinity {
    Affinity {
        pod_affinity: Some(PodAffinity {
            preferred_during_scheduling_ignored_during_execution: Some(vec![
                WeightedPodAffinityTerm {
                    weight: 100,
                    pod_affinity_term: PodAffinityTerm {
                        label_selector: Some(LabelSelector {
                            match_labels: Some(selector_labels_map.clone()),
                            ..Default::default()
                        }),
                        topology_key: "kubernetes.io/hostname".to_string(),
                        ..Default::default()
                    },
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Resolve the pod-level `securityContext` from the structured passthrough
/// (`spec.securityContext`) plus the legacy top-level `spec.fsGroup`, folded
/// into ONE `PodSecurityContext`.
///
/// - Neither set ⇒ `None`: the pod carries no `securityContext` (byte-identical
///   to a CR that predates these fields).
/// - Only the legacy `fsGroup` set ⇒ exactly `PodSecurityContext { fs_group:
///   Some(N), .. }` — byte-identical to the pre-passthrough behavior.
/// - Structured context set ⇒ its fields, with the legacy `fsGroup` folded in
///   ONLY when the structured context omits `fsGroup` (structured wins).
///
/// Whenever an `fsGroup` ends up in effect, `fsGroupChangePolicy` defaults to
/// `OnRootMismatch` unless the CR states one. WHY, and why here:
///
/// `hanzo-git` (git.hanzo.ai, the canonical forge for the whole estate) served
/// 503 for several minutes with its pod in `Init:0/1` while the kubelet logged
/// `VolumePermissionChangeInProgress … is taking longer than expected, consider
/// using OnRootMismatch`. It carries `securityContext: {fsGroup: 1000}` over a
/// 250Gi PVC holding a git forge — millions of tiny loose objects — and k8s
/// defaults `fsGroupChangePolicy` to `Always`, so the kubelet recursively
/// chowned EVERY file before the container could start, restarting the walk from
/// zero on each ReplicaSet roll. The cost is paid on every restart forever, so
/// any service whose volume grows large enough becomes un-restartable.
///
/// `Always` is the k8s default, but it is the wrong default HERE: an `fsGroup`
/// is declared in this operator for exactly one reason — a non-root image must
/// write a persistence PVC — so the population that sets it IS the population of
/// long-lived volumes that `Always` degrades without bound, and it degrades
/// silently until it takes an outage. Opt-in would mean every such service pays
/// one outage before someone thinks to set the field. The changeover is cheap:
/// `Always` has already left the volume root owned by the fsGroup, so the first
/// roll under `OnRootMismatch` matches on the root check and skips the walk.
///
/// What this gives up: `Always` also repairs files DEEP in a volume whose
/// ownership drifted (a restore that dropped root-owned files in). That is not a
/// property a workload should depend on, and it stays one explicit field away.
pub fn pod_security_context(
    ctx: Option<&crate::crd_types::PodSecurityContext>,
    legacy_fs_group: Option<i64>,
) -> Option<PodSecurityContext> {
    let mut k = match ctx {
        // No structured context ⇒ the legacy `fsGroup` alone, or nothing at all.
        None => PodSecurityContext {
            fs_group: Some(legacy_fs_group?),
            ..Default::default()
        },
        Some(c) => {
            let mut k = c.to_k8s();
            k.fs_group = k.fs_group.or(legacy_fs_group);
            k
        }
    };
    // No fsGroup ⇒ the kubelet never chowns, so a policy would be dead weight on
    // the PodSpec (and a needless diff for every hardening-only CR).
    if k.fs_group.is_some() && k.fs_group_change_policy.is_none() {
        let skip_the_walk = crate::crd_types::FsGroupChangePolicy::OnRootMismatch;
        k.fs_group_change_policy = Some(skip_the_walk.as_str().to_string());
    }
    Some(k)
}

/// Build a StatefulSet.
#[allow(clippy::too_many_arguments)]
pub fn build_statefulset(
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    selector_labels_map: BTreeMap<String, String>,
    replicas: Option<i32>,
    containers: Vec<Container>,
    volumes: Vec<Volume>,
    pvc_templates: Vec<PersistentVolumeClaim>,
    image_pull_secrets: Vec<LocalObjectReference>,
    service_name: &str,
) -> StatefulSet {
    let containers = inject_pre_stop(containers);

    StatefulSet {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            replicas,
            min_ready_seconds: Some(10),
            // k8s 1.33: StatefulSetSpec.service_name is now Option<String>.
            service_name: Some(service_name.to_string()),
            update_strategy: Some(StatefulSetUpdateStrategy {
                type_: Some("RollingUpdate".to_string()),
                ..Default::default()
            }),
            selector: LabelSelector {
                match_labels: Some(selector_labels_map),
                ..Default::default()
            },
            volume_claim_templates: if pvc_templates.is_empty() {
                None
            } else {
                Some(pvc_templates)
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers,
                    volumes: if volumes.is_empty() {
                        None
                    } else {
                        Some(volumes)
                    },
                    image_pull_secrets: if image_pull_secrets.is_empty() {
                        None
                    } else {
                        Some(image_pull_secrets)
                    },
                    termination_grace_period_seconds: Some(30),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a ClusterIP Service.
pub fn build_service(
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    ports: Vec<ServicePort>,
    selector_labels_map: BTreeMap<String, String>,
) -> CoreService {
    CoreService {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(CoreServiceSpec {
            type_: Some("ClusterIP".to_string()),
            selector: Some(selector_labels_map),
            ports: Some(ports),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a headless Service (`ClusterIP: None`) for StatefulSet pod DNS.
pub fn build_headless_service(
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    ports: Vec<ServicePort>,
    selector_labels_map: BTreeMap<String, String>,
) -> CoreService {
    let mut svc = build_service(name, namespace, labels, ports, selector_labels_map);
    if let Some(s) = svc.spec.as_mut() {
        s.cluster_ip = Some("None".to_string());
    }
    svc
}

/// Build an Ingress with cert-manager annotations.
pub fn build_ingress(
    name: &str,
    namespace: &str,
    spec: &IngressSpec,
    service_name: &str,
    service_port: i32,
    labels: BTreeMap<String, String>,
) -> Ingress {
    let mut annotations: BTreeMap<String, String> = BTreeMap::new();
    if spec.tls {
        let issuer = if spec.cluster_issuer.is_empty() {
            "letsencrypt-prod"
        } else {
            &spec.cluster_issuer
        };
        annotations.insert(
            "cert-manager.io/cluster-issuer".to_string(),
            issuer.to_string(),
        );
    }
    if let Some(ann) = &spec.annotations {
        for (k, v) in ann {
            annotations.insert(k.clone(), v.clone());
        }
    }
    // Always in the annotation form, never spec.ingressClassName: the field takes
    // precedence over the annotation and then fails the IngressClass controller
    // check, so hanzoai/ingress (Traefik fork) serves nothing and drops spec.tls.
    //
    // And always present. This used to be emitted only when a CR set
    // `ingressClassName`, but that field defaults to "", so every App that just
    // said `ingress: {enabled: true}` got an Ingress with no class at all — which
    // matches no provider either. That is how hanzo-devnet/{cloud-api,commerce,
    // console2,iam} and hanzo-testnet/{cloud-api,iam} sat dark for a month,
    // 404ing with router "-" while their Services had ready endpoints.
    //
    // Precedence: the explicit field, else an operator-supplied annotation, else
    // the default. The one thing that cannot happen is no class.
    if spec.ingress_class_name.is_empty() {
        annotations
            .entry("kubernetes.io/ingress.class".to_string())
            .or_insert_with(|| DEFAULT_INGRESS_CLASS.to_string());
    } else {
        annotations.insert(
            "kubernetes.io/ingress.class".to_string(),
            spec.ingress_class_name.clone(),
        );
    }

    let path_type = "Prefix".to_string();
    let mut rules = Vec::new();
    for host in &spec.hosts {
        // The implicit "/" is a DEFAULT, not an addition: it applies only when the
        // CR declares no pathRules. Emitting it unconditionally and then appending
        // the explicit rules produced TWO "/" paths, and the implicit one — pinned
        // to the app's FIRST service port — won. `dns` declares `/ -> dns:8443` yet
        // its first port is 53, so dns.hanzo.ai routed HTTP at the DNS port and
        // served a bare 404. Explicit configuration wins over a default.
        let mut paths = if spec.path_rules.is_empty() {
            vec![HTTPIngressPath {
                path: Some("/".to_string()),
                path_type: path_type.clone(),
                backend: IngressBackend {
                    service: Some(IngressServiceBackend {
                        name: service_name.to_string(),
                        port: Some(ServiceBackendPort {
                            number: Some(service_port),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                },
            }]
        } else {
            Vec::new()
        };

        for pr in &spec.path_rules {
            let pt = match pr.path_type.as_str() {
                "Exact" => "Exact".to_string(),
                "ImplementationSpecific" => "ImplementationSpecific".to_string(),
                _ => "Prefix".to_string(),
            };
            let backend_name = if pr.service_name.is_empty() {
                service_name
            } else {
                &pr.service_name
            };
            paths.push(HTTPIngressPath {
                path: Some(pr.path.clone()),
                path_type: pt,
                backend: IngressBackend {
                    service: Some(IngressServiceBackend {
                        name: backend_name.to_string(),
                        port: Some(ServiceBackendPort {
                            number: Some(pr.port),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                },
            });
        }

        rules.push(IngressRule {
            host: Some(host.clone()),
            http: Some(HTTPIngressRuleValue { paths }),
        });
    }

    let tls = if spec.tls && !spec.hosts.is_empty() {
        Some(vec![IngressTLS {
            hosts: Some(spec.hosts.clone()),
            secret_name: Some(format!("{}-tls", name)),
        }])
    } else {
        None
    };

    Ingress {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            annotations: Some(annotations),
            ..Default::default()
        },
        spec: Some(K8sIngressSpec {
            rules: Some(rules),
            tls,
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build an HPA targeting a Deployment.
pub fn build_hpa(
    name: &str,
    namespace: &str,
    target_ref: CrossVersionObjectReference,
    spec: &AutoscalingSpec,
    labels: BTreeMap<String, String>,
) -> HorizontalPodAutoscaler {
    let mut metrics = Vec::new();

    if let Some(cpu) = spec.target_cpu_utilization {
        metrics.push(MetricSpec {
            type_: "Resource".to_string(),
            resource: Some(ResourceMetricSource {
                name: "cpu".to_string(),
                target: MetricTarget {
                    type_: "Utilization".to_string(),
                    average_utilization: Some(cpu),
                    ..Default::default()
                },
            }),
            ..Default::default()
        });
    }
    if let Some(mem) = spec.target_memory_utilization {
        metrics.push(MetricSpec {
            type_: "Resource".to_string(),
            resource: Some(ResourceMetricSource {
                name: "memory".to_string(),
                target: MetricTarget {
                    type_: "Utilization".to_string(),
                    average_utilization: Some(mem),
                    ..Default::default()
                },
            }),
            ..Default::default()
        });
    }

    HorizontalPodAutoscaler {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(HorizontalPodAutoscalerSpec {
            scale_target_ref: target_ref,
            min_replicas: spec.min_replicas,
            max_replicas: spec.max_replicas.unwrap_or(10),
            metrics: Some(metrics),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build a PodDisruptionBudget.
pub fn build_pdb(
    name: &str,
    namespace: &str,
    spec: &PodDisruptionBudgetSpec,
    selector_labels_map: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
) -> PodDisruptionBudget {
    let mut pdb_spec = K8sPDBSpec {
        selector: Some(LabelSelector {
            match_labels: Some(selector_labels_map),
            ..Default::default()
        }),
        ..Default::default()
    };
    if let Some(min) = spec.min_available {
        pdb_spec.min_available = Some(IntOrString::Int(min));
    } else if let Some(max) = spec.max_unavailable {
        pdb_spec.max_unavailable = Some(IntOrString::Int(max));
    } else {
        pdb_spec.min_available = Some(IntOrString::Int(1));
    }
    PodDisruptionBudget {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(pdb_spec),
        ..Default::default()
    }
}

/// Build a NetworkPolicy.
pub fn build_network_policy(
    name: &str,
    namespace: &str,
    spec: &NetworkPolicySpec,
    selector_labels_map: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
) -> NetworkPolicy {
    let mut from: Vec<K8sNetworkPolicyPeer> = Vec::new();

    let allow_intra = spec.allow_intra_namespace.unwrap_or(true);
    if allow_intra {
        from.push(K8sNetworkPolicyPeer {
            pod_selector: Some(LabelSelector::default()),
            ..Default::default()
        });
    }
    for peer in &spec.allow_from {
        from.push(K8sNetworkPolicyPeer {
            pod_selector: peer.pod_selector.as_ref().map(|s| LabelSelector {
                match_labels: s.match_labels.clone(),
                ..Default::default()
            }),
            namespace_selector: peer.namespace_selector.as_ref().map(|s| LabelSelector {
                match_labels: s.match_labels.clone(),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    let ingress = if spec.allow_ingress {
        Some(vec![NetworkPolicyIngressRule::default()])
    } else if !from.is_empty() {
        Some(vec![NetworkPolicyIngressRule {
            from: Some(from),
            ..Default::default()
        }])
    } else {
        None
    };

    NetworkPolicy {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(K8sNetworkPolicySpec {
            // k8s 1.33: NetworkPolicySpec.pod_selector is now Option<LabelSelector>.
            pod_selector: Some(LabelSelector {
                match_labels: Some(selector_labels_map),
                ..Default::default()
            }),
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress,
            ..Default::default()
        }),
    }
}

/// The tag of an image reference, when it has one.
///
/// `ghcr.io/hanzoai/cloud:v1.2.3` → `v1.2.3`. Returns None for a digest pin
/// (`repo@sha256:…`) and for an untagged `repo`, and is not fooled by a registry
/// port (`localhost:5000/repo`), where the colon precedes the last slash.
fn image_tag(image: &str) -> Option<&str> {
    let base = image.split('@').next().unwrap_or(image);
    let colon = base.rfind(':')?;
    let tag = &base[colon + 1..];
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(tag)
}

/// Report the image tag a container was rendered from as `HANZO_VERSION`.
///
/// A workload that cannot name its own build makes a rollout unverifiable from
/// outside: a fresh deploy and a pod that never restarted answer identically.
/// Sourcing it here means it can never drift from `image`, and it costs no
/// rebuild — the release assigns the final version only after the image is
/// pushed, so a link-time stamp would race it.
///
/// An explicit `HANZO_VERSION` in the spec wins; the operator does not overwrite
/// what an author set deliberately.
fn with_version_env(mut env: Vec<EnvVar>, image: &str) -> Vec<EnvVar> {
    if env.iter().any(|e| e.name == "HANZO_VERSION") {
        return env;
    }
    if let Some(tag) = image_tag(image) {
        env.push(EnvVar {
            name: "HANZO_VERSION".to_string(),
            value: Some(tag.to_string()),
            ..Default::default()
        });
    }
    env
}

/// Build a single container with image+ports+env+volumes+probes wired.
#[allow(clippy::too_many_arguments)]
pub fn build_container(
    name: &str,
    image: &str,
    image_pull_policy: &str,
    command: Vec<String>,
    args: Vec<String>,
    env: Vec<EnvVar>,
    env_from: Vec<EnvFromSource>,
    volume_mounts: Vec<VolumeMount>,
    ports: Vec<ContainerPort>,
    resources: Option<K8sResourceRequirements>,
    liveness_probe: Option<Probe>,
    readiness_probe: Option<Probe>,
) -> Container {
    let env = with_version_env(env, image);
    Container {
        name: name.to_string(),
        image: Some(image.to_string()),
        image_pull_policy: if image_pull_policy.is_empty() {
            None
        } else {
            Some(image_pull_policy.to_string())
        },
        command: if command.is_empty() {
            None
        } else {
            Some(command)
        },
        args: if args.is_empty() { None } else { Some(args) },
        env: if env.is_empty() { None } else { Some(env) },
        env_from: if env_from.is_empty() {
            None
        } else {
            Some(env_from)
        },
        volume_mounts: if volume_mounts.is_empty() {
            None
        } else {
            Some(volume_mounts)
        },
        ports: if ports.is_empty() { None } else { Some(ports) },
        resources,
        liveness_probe,
        readiness_probe,
        ..Default::default()
    }
}

/// Build a ConfigMap from a `key -> contents` map.
pub fn build_configmap(
    name: &str,
    namespace: &str,
    labels: BTreeMap<String, String>,
    data: BTreeMap<String, String>,
) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            labels: Some(labels),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    }
}

/// True when a ConfigMap carries no config — both `data` and `binaryData`
/// are absent or empty. Such a ConfigMap must NEVER be force-applied: SSA
/// would strip every key the operator's field manager owns, blanking a
/// mounted config file and crashlooping the workload. `apply::apply_configmap`
/// enforces this gate (root cause of the hanzo.id auth outage: `iam-conf`
/// regenerated empty → `panic: unable to open database file`).
pub fn configmap_is_empty(cm: &ConfigMap) -> bool {
    let data_empty = cm.data.as_ref().map_or(true, |d| d.is_empty());
    let binary_empty = cm.binary_data.as_ref().map_or(true, |d| d.is_empty());
    data_empty && binary_empty
}

/// Resolve image repository + tag into a single image reference.
pub fn image_ref(repository: &str, tag: &str) -> String {
    if tag.is_empty() {
        repository.to_string()
    } else {
        format!("{}:{}", repository, tag)
    }
}

/// Compute the primary service port (first defined) — used for default
/// Ingress backend.
pub fn primary_port(ports: &[CrServicePort]) -> i32 {
    if let Some(p) = ports.first() {
        p.service_port.unwrap_or(p.container_port)
    } else {
        80
    }
}

/// Build a PersistentVolumeClaim template for a StatefulSet.
pub fn build_pvc_template(name: &str, storage_class: &str, size: &str) -> PersistentVolumeClaim {
    // k8s 1.33: PersistentVolumeClaimSpec.resources is now the dedicated
    // VolumeResourceRequirements type (requests/limits only, no `claims`).
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimSpec, VolumeResourceRequirements};
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            storage_class_name: if storage_class.is_empty() {
                Some("do-block-storage".to_string())
            } else {
                Some(storage_class.to_string())
            },
            resources: Some(VolumeResourceRequirements {
                requests: Some({
                    let mut m = BTreeMap::new();
                    m.insert("storage".to_string(), Quantity(size.to_string()));
                    m
                }),
                limits: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{ExecAction as CrExec, TcpSocketAction as CrTcp};

    fn deploy_with(placement: Placement) -> Deployment {
        build_deployment(
            "cloud",
            "hanzo",
            standard_labels("cloud", "", "", "v1"),
            selector_labels("cloud"),
            Some(1),
            vec![],
            vec![],
            "Recreate",
            vec![],
            "",
            placement,
        )
    }

    /// THE NO-OP CLAIM. An App that says nothing about placement must render the
    /// Deployment it rendered before these fields existed — not "almost", not
    /// "semantically equivalent": the same bytes.
    ///
    /// Asserted structurally (the three fields are absent, so they serialize
    /// away entirely under `skip_serializing_if`) rather than by eyeballing a
    /// golden file, so it stays true as the rest of the PodSpec evolves.
    #[test]
    fn absent_placement_renders_a_byte_identical_deployment() {
        let before = serde_json::to_value(deploy_with(Placement::default())).unwrap();

        // The three keys must not appear ANYWHERE in the rendered object — an
        // explicit `null` would still be a wire change on a server-side apply.
        let s = serde_json::to_string(&before).unwrap();
        for key in ["nodeSelector", "tolerations", "priorityClassName"] {
            assert!(
                !s.contains(key),
                "an App with no placement must not render `{key}` at all — found it in {s}"
            );
        }

        let pod = before["spec"]["template"]["spec"].clone();
        assert!(pod.get("nodeSelector").is_none());
        assert!(pod.get("tolerations").is_none());
        assert!(pod.get("priorityClassName").is_none());

        // And the whole object is identical to the same build repeated, i.e. the
        // new parameter introduced no nondeterminism.
        assert_eq!(
            before,
            serde_json::to_value(deploy_with(Placement::default())).unwrap()
        );
    }

    /// An all-empty `Placement` built from an omitting CR is `is_empty`, and a
    /// `nodeSelector: {}` (present but empty) is treated as no opinion too — it
    /// must not render an empty map onto the PodSpec.
    #[test]
    fn empty_node_selector_map_is_still_no_placement() {
        assert!(Placement::default().is_empty());
        let p = Placement {
            node_selector: Some(BTreeMap::new()),
            ..Default::default()
        };
        assert!(p.is_empty(), "an empty map is no opinion");
        let d = deploy_with(p);
        let pod = d.spec.unwrap().template.spec.unwrap();
        assert!(
            pod.node_selector.is_none(),
            "an empty nodeSelector map must render as absent, not `{{}}`"
        );
    }

    /// The whole point: when a CR DOES state placement, all three reach the
    /// PodSpec verbatim — this is what makes the writer's seat a reservation
    /// instead of a race.
    #[test]
    fn stated_placement_reaches_the_pod_spec() {
        let mut ns = BTreeMap::new();
        ns.insert("hanzo.ai/pool".to_string(), "writer".to_string());
        let d = deploy_with(Placement {
            node_selector: Some(ns),
            tolerations: vec![Toleration {
                key: Some("dedicated".to_string()),
                operator: Some("Equal".to_string()),
                value: Some("writer".to_string()),
                effect: Some("NoSchedule".to_string()),
                ..Default::default()
            }],
            priority_class_name: "hanzo-writer".to_string(),
        });
        let pod = d.spec.unwrap().template.spec.unwrap();
        assert_eq!(
            pod.node_selector.unwrap().get("hanzo.ai/pool").unwrap(),
            "writer"
        );
        let tol = pod.tolerations.expect("tolerations must reach the PodSpec");
        assert_eq!(tol.len(), 1);
        assert_eq!(tol[0].key.as_deref(), Some("dedicated"));
        assert_eq!(tol[0].effect.as_deref(), Some("NoSchedule"));
        assert_eq!(
            pod.priority_class_name.as_deref(),
            Some("hanzo-writer"),
            "priorityClassName is what preempts a squatter"
        );
    }

    fn probe(port: i32) -> ProbeSpec {
        ProbeSpec {
            path: String::new(),
            port,
            exec: None,
            tcp_socket: None,
            initial_delay_seconds: 0,
            period_seconds: 0,
        }
    }

    // An `exec` probe (Postgres `pg_isready`, Valkey `redis-cli ping`) renders as
    // an exec handler — NOT a mangled `httpGet{port:0}` — and carries no other
    // handler.
    #[test]
    fn build_probe_renders_exec_handler() {
        let mut p = probe(0);
        p.exec = Some(CrExec {
            command: vec!["pg_isready".into(), "-U".into(), "hanzo".into()],
        });
        let out = build_probe(&p).expect("exec probe must render");
        assert_eq!(
            out.exec.unwrap().command.unwrap(),
            vec!["pg_isready", "-U", "hanzo"]
        );
        assert!(out.http_get.is_none(), "exec probe must not emit httpGet");
        assert!(out.tcp_socket.is_none());
    }

    // A `tcpSocket` probe (Kafka TCP :9092) renders as a tcpSocket handler with
    // the right port and no httpGet.
    #[test]
    fn build_probe_renders_tcp_socket_handler() {
        let mut p = probe(0);
        p.tcp_socket = Some(CrTcp { port: 9092 });
        let out = build_probe(&p).expect("tcp probe must render");
        assert_eq!(out.tcp_socket.unwrap().port, IntOrString::Int(9092));
        assert!(out.http_get.is_none(), "tcp probe must not emit httpGet");
    }

    // Explicit pathRules are AUTHORITATIVE: the implicit "/" is a default that
    // applies only when none are declared. Emitting both gave two "/" paths and
    // the implicit one won, pinned to the app's FIRST service port — which routed
    // dns.hanzo.ai's HTTP at port 53 (DNS) and served a bare 404.
    #[test]
    fn explicit_path_rules_replace_the_implicit_root_default() {
        fn ports_of(ing: &Ingress) -> Vec<i32> {
            let rules = ing.spec.as_ref().unwrap().rules.as_ref().unwrap();
            let paths = &rules[0].http.as_ref().unwrap().paths;
            paths
                .iter()
                .filter_map(|p| p.backend.service.as_ref()?.port.as_ref()?.number)
                .collect()
        }

        let mut ing = crate::crd::IngressSpec {
            enabled: true,
            hosts: vec!["dns.hanzo.ai".into()],
            ..Default::default()
        };

        // No rules -> exactly the default root, at the given service port.
        let out = build_ingress("dns", "hanzo", &ing, "dns", 53, BTreeMap::new());
        assert_eq!(ports_of(&out), vec![53]);

        // With an explicit root, the default must NOT also be emitted.
        ing.path_rules = vec![crate::crd::PathRule {
            path: "/".into(),
            path_type: "Prefix".into(),
            port: 8443,
            service_name: "dns".into(),
        }];
        let out = build_ingress("dns", "hanzo", &ing, "dns", 53, BTreeMap::new());
        assert_eq!(
            ports_of(&out),
            vec![8443],
            "declared port wins; the first service port must not also be routed",
        );
    }

    // An Ingress with no class matches no provider under
    // --providers.kubernetesingress.ingressclass=ingress: no router, host 404s,
    // and nothing anywhere says so. Six live Ingresses (hanzo-devnet/{cloud-api,
    // commerce,console2,iam}, hanzo-testnet/{cloud-api,iam}) were dark exactly
    // this way because their App CRs never set `ingressClassName`. The class must
    // survive every path through build_ingress.
    #[test]
    fn every_ingress_carries_a_class() {
        fn class_of(ing: &Ingress) -> Option<String> {
            ing.metadata
                .annotations
                .as_ref()?
                .get("kubernetes.io/ingress.class")
                .cloned()
        }

        // The regression: a CR that names no class at all.
        let bare = crate::crd::IngressSpec {
            enabled: true,
            hosts: vec!["api.devnet.hanzo.ai".into()],
            ..Default::default()
        };
        assert_eq!(
            class_of(&build_ingress(
                "cloud-api",
                "hanzo-devnet",
                &bare,
                "cloud-api",
                8000,
                BTreeMap::new()
            ))
            .as_deref(),
            Some(DEFAULT_INGRESS_CLASS),
            "an App that omits ingressClassName must still get a routable class",
        );

        // An explicit class still wins.
        let explicit = crate::crd::IngressSpec {
            ingress_class_name: "gateway".into(),
            ..bare.clone()
        };
        assert_eq!(
            class_of(&build_ingress(
                "bot",
                "hanzo",
                &explicit,
                "bot",
                80,
                BTreeMap::new()
            ))
            .as_deref(),
            Some("gateway"),
        );

        // So does an operator-supplied annotation, when the field is empty —
        // defaulting must not clobber a deliberate override.
        let annotated = crate::crd::IngressSpec {
            annotations: Some(BTreeMap::from([(
                "kubernetes.io/ingress.class".to_string(),
                "gateway".to_string(),
            )])),
            ..bare.clone()
        };
        assert_eq!(
            class_of(&build_ingress(
                "bot",
                "hanzo",
                &annotated,
                "bot",
                80,
                BTreeMap::new()
            ))
            .as_deref(),
            Some("gateway"),
        );

        // The class is an annotation, never spec.ingressClassName: the field
        // takes precedence and then fails the IngressClass controller check.
        for spec in [&bare, &explicit, &annotated] {
            let out = build_ingress("x", "hanzo", spec, "x", 80, BTreeMap::new());
            assert_eq!(
                out.spec.as_ref().unwrap().ingress_class_name,
                None,
                "spec.ingressClassName serves nothing; the class belongs in the annotation",
            );
        }
    }

    // A DECLARED probe must never get a stricter budget than the one the
    // operator supplies for a CR that declares nothing. Leaving these to the
    // k8s defaults (1s timeout, 3 failures) restarts healthy single-writer
    // services that block for a second under load.
    #[test]
    fn declared_probe_gets_the_same_lenient_floor_as_the_default() {
        let floor = default_readiness_probe(&[svc_port("http", 3000)])
            .expect("default probe must render for a ported workload");

        let mut tcp = probe(0);
        tcp.tcp_socket = Some(CrTcp { port: 3000 });
        for out in [
            build_probe(&probe(3000)).expect("http probe must render"),
            build_probe(&tcp).expect("tcp probe must render"),
        ] {
            assert_eq!(out.timeout_seconds, floor.timeout_seconds);
            assert_eq!(out.failure_threshold, floor.failure_threshold);
        }
    }

    // A plain HTTP probe (port > 0) still renders as httpGet.
    #[test]
    fn build_probe_renders_http_handler() {
        let out = build_probe(&probe(7700)).expect("http probe must render");
        let hg = out.http_get.expect("http probe must emit httpGet");
        assert_eq!(hg.port, IntOrString::Int(7700));
        assert!(out.exec.is_none() && out.tcp_socket.is_none());
    }

    // The ClusterIP Service builder must NEVER put clusterIP in the desired
    // manifest. clusterIP is immutable and apiserver-assigned; a server-side
    // apply that omits it lets the apiserver keep the live value, so adopting a
    // Service across the `Service` CR → `App` handoff preserves its identity
    // (same clusterIP, no Endpoint/DNS re-propagation gap). Emitting it would
    // invite an immutable-field conflict → recreate → the ~50s cutover outage.
    #[test]
    fn build_service_omits_clusterip_so_ssa_preserves_the_live_one() {
        let svc = build_service("chat", "hanzo", BTreeMap::new(), vec![], BTreeMap::new());
        let spec = svc.spec.as_ref().expect("service has a spec");
        assert_eq!(spec.type_.as_deref(), Some("ClusterIP"));
        assert!(
            spec.cluster_ip.is_none(),
            "clusterIP is apiserver-owned; it must never be in the desired manifest"
        );
        assert!(spec.cluster_ips.is_none(), "clusterIPs must be omitted too");
        // The operative property that makes SSA byte-stable on adoption: the
        // serialized object carries no clusterIP key, so the apply never sets or
        // changes the immutable field.
        let json = serde_json::to_string(&svc).expect("service serializes");
        assert!(
            !json.contains("clusterIP"),
            "serialized Service must not contain clusterIP: {json}"
        );
    }

    // A headless Service, by contrast, DECLARES clusterIP:None — that sentinel is
    // its identity and must be emitted. It is a distinct name (`*-hs`) from the
    // ClusterIP Service, so the two never collide and flip None ↔ assigned.
    #[test]
    fn build_headless_service_declares_the_none_sentinel() {
        let hs =
            build_headless_service("sql-hs", "hanzo", BTreeMap::new(), vec![], BTreeMap::new());
        assert_eq!(
            hs.spec.as_ref().and_then(|s| s.cluster_ip.as_deref()),
            Some("None"),
            "a headless Service's identity IS clusterIP:None — it must be sent"
        );
    }

    // The regression guard: a probe with NO usable handler (port 0, no
    // exec/tcpSocket) renders NOTHING rather than an invalid `httpGet{port:0}`
    // the API server rejects — the root of the 33 err/min reconcile storm.
    #[test]
    fn build_probe_never_emits_port_zero_http() {
        assert!(
            build_probe(&probe(0)).is_none(),
            "an empty probe must yield None, never httpGet{{port:0}}"
        );
    }

    fn svc_port(name: &str, container_port: i32) -> CrServicePort {
        CrServicePort {
            name: name.to_string(),
            container_port,
            service_port: None,
            protocol: String::new(),
        }
    }

    // An App whose CR OMITS readinessProbe but exposes a port gets a DEFAULTED
    // TCP-socket probe on the FIRST container port, so maxUnavailable=0 actually
    // gates the roll: a broken image that never listens is kept out of `Ready`
    // and cannot roll over the healthy pod. TCP, not HTTP — an HTTP default
    // would 404 services without that path and stall GOOD rolls fleet-wide.
    #[test]
    fn default_readiness_probe_tcp_on_first_port() {
        let ports = vec![svc_port("http", 8080), svc_port("metrics", 9090)];
        let out =
            default_readiness_probe(&ports).expect("a port-bearing workload gets a default probe");
        assert_eq!(
            out.tcp_socket
                .expect("default must be a tcpSocket probe")
                .port,
            IntOrString::Int(8080),
            "default probe must target the FIRST container port",
        );
        assert!(
            out.http_get.is_none(),
            "default must be TCP, never HTTP (an HTTP 404 stalls good rolls)"
        );
        assert!(out.exec.is_none());
        // Lenient fleet-wide floor: ~70s grace (initialDelay 10 + 6×period 10)
        // so slow-boot starters don't stall a good roll.
        assert_eq!(out.initial_delay_seconds, Some(10));
        assert_eq!(out.period_seconds, Some(10));
        assert_eq!(out.timeout_seconds, Some(3));
        assert_eq!(out.failure_threshold, Some(6));
        assert_eq!(out.success_threshold, Some(1));
    }

    // A port-less worker (a queue consumer with no listener) gets NO default
    // probe — nothing to TCP-probe — so its Deployment stays byte-identical.
    #[test]
    fn default_readiness_probe_none_for_portless_worker() {
        assert!(
            default_readiness_probe(&[]).is_none(),
            "a port-less workload must get no default probe",
        );
    }

    // Handler precedence is exec > tcpSocket > httpGet: a CR that (wrongly)
    // sets several picks exactly one, so the object is never rejected for
    // specifying more than one handler type.
    #[test]
    fn build_probe_handler_precedence_is_exec_then_tcp_then_http() {
        let mut p = probe(8080);
        p.tcp_socket = Some(CrTcp { port: 9092 });
        p.exec = Some(CrExec {
            command: vec!["true".into()],
        });
        let out = build_probe(&p).unwrap();
        assert!(out.exec.is_some());
        assert!(out.tcp_socket.is_none() && out.http_get.is_none());

        let mut p2 = probe(8080);
        p2.tcp_socket = Some(CrTcp { port: 9092 });
        let out2 = build_probe(&p2).unwrap();
        assert!(out2.tcp_socket.is_some() && out2.http_get.is_none());
    }

    // The console regression: a digest-pinned image tag (now canonical per
    // universe#445) must NOT land verbatim in a label — it exceeds 63 chars and
    // contains illegal `@`/`:`, which rejected the whole Deployment apply.
    #[test]
    fn sanitize_label_value_strips_digest_from_pinned_tag() {
        let v = "v8.4.118@sha256:9820e1539f1a51c36179a595fda500c9470461e9b2ea0e42c7166decbc70b77a";
        assert_eq!(sanitize_label_value(v), "v8.4.118");
        assert!(is_valid_label_value(&sanitize_label_value(v)));
    }

    // A plain semver tag is a valid label value and passes through unchanged.
    #[test]
    fn sanitize_label_value_passes_plain_tags_through() {
        for t in ["18", "0.1.1", "v2.7.1", "latest"] {
            assert_eq!(sanitize_label_value(t), t, "plain tag must be unchanged");
        }
    }

    // A bare-digest ref (no human tag before `@`) folds to a valid `sha256-…`.
    #[test]
    fn sanitize_label_value_folds_bare_digest() {
        let out = sanitize_label_value(
            "@sha256:9820e1539f1a51c36179a595fda500c9470461e9b2ea0e42c7166decbc70b77a",
        );
        assert!(out.starts_with("sha256-"));
        assert!(
            is_valid_label_value(&out),
            "bare digest must sanitize valid: {out}"
        );
    }

    // Any long/illegal value is capped at 63 chars and trimmed to an
    // alphanumeric boundary — the two hard label constraints.
    #[test]
    fn sanitize_label_value_caps_length_and_boundaries() {
        let out = sanitize_label_value(&format!("v1.2.3@{}", "a".repeat(200)));
        assert_eq!(out, "v1.2.3");
        // A value that is illegal chars + long still ends valid.
        let messy = sanitize_label_value(&"_-.".repeat(30));
        assert!(is_valid_label_value(&messy) || messy.is_empty());
    }

    // The end-to-end guard: standard_labels emits a VALID version label for a
    // digest-pinned image (previously the FieldValueInvalid on console).
    #[test]
    fn standard_labels_version_is_a_valid_label_for_pinned_image() {
        let l = standard_labels(
            "console",
            "app",
            "cloud",
            "v8.4.118@sha256:9820e1539f1a51c36179a595fda500c9470461e9b2ea0e42c7166decbc70b77a",
        );
        let ver = l.get(LABEL_VERSION).expect("version label present");
        assert_eq!(ver, "v8.4.118");
        assert!(is_valid_label_value(ver));
    }

    /// Mirror of the k8s label-value validation (RFC 1123-ish): ≤63 chars,
    /// `[A-Za-z0-9._-]`, start + end alphanumeric.
    fn is_valid_label_value(v: &str) -> bool {
        !v.is_empty()
            && v.len() <= 63
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            && v.chars().next().unwrap().is_ascii_alphanumeric()
            && v.chars().last().unwrap().is_ascii_alphanumeric()
    }

    /// The outage guard: a ConfigMap built from an empty CR config source is
    /// flagged empty so `apply::apply_configmap` skips it and leaves any
    /// existing populated ConfigMap untouched.
    #[test]
    fn empty_config_source_yields_empty_configmap_flag() {
        let cm = build_configmap("iam-conf", "hanzo", BTreeMap::new(), BTreeMap::new());
        assert!(
            configmap_is_empty(&cm),
            "empty-data ConfigMap must be flagged empty so the apply guard skips it"
        );
    }

    #[test]
    fn populated_config_source_not_empty() {
        let mut data = BTreeMap::new();
        data.insert("app.conf".to_string(), "listen = :8000\n".to_string());
        let cm = build_configmap("iam-conf", "hanzo", BTreeMap::new(), data);
        assert!(
            !configmap_is_empty(&cm),
            "populated ConfigMap must apply normally"
        );
    }

    #[test]
    fn binary_only_configmap_not_empty() {
        use k8s_openapi::ByteString;
        let mut cm = build_configmap("bin-conf", "hanzo", BTreeMap::new(), BTreeMap::new());
        let mut bd = BTreeMap::new();
        bd.insert("blob".to_string(), ByteString(vec![1, 2, 3]));
        cm.binary_data = Some(bd);
        assert!(
            !configmap_is_empty(&cm),
            "binary-only ConfigMap carries config and must apply"
        );
    }

    // ---- pod_security_context: the structured passthrough + legacy fsGroup fold ----

    use crate::crd_types::{PodSecurityContext as CrPodSc, SeccompProfile as CrSeccomp};

    /// Neither set ⇒ no securityContext — a CR predating these fields is untouched.
    #[test]
    fn pod_security_context_absent_is_none() {
        assert!(pod_security_context(None, None).is_none());
    }

    /// ONLY the legacy top-level fsGroup ⇒ exactly `{fsGroup: N,
    /// fsGroupChangePolicy: OnRootMismatch}` — the fsGroup as before, plus the
    /// default that keeps the kubelet from re-walking the whole volume.
    #[test]
    fn pod_security_context_legacy_fs_group_only_renders_fs_group_and_the_skip_policy() {
        let got = pod_security_context(None, Some(1001)).expect("fsGroup must render");
        assert_eq!(
            got,
            PodSecurityContext {
                fs_group: Some(1001),
                fs_group_change_policy: Some("OnRootMismatch".to_string()),
                ..Default::default()
            },
            "a legacy fsGroup must render fsGroup + the OnRootMismatch default and nothing else"
        );
    }

    /// THE INCIDENT TEST. `hanzo-git` (git.hanzo.ai) served 503 for minutes with
    /// its pod stuck in `Init:0/1` while the kubelet logged
    /// `VolumePermissionChangeInProgress … consider using OnRootMismatch`: with
    /// `fsGroup: 1000` and no policy, k8s defaults to `Always` and recursively
    /// chowns every file on the 250Gi PVC (`pvc-47211c5d-3183-4583-8357-3a426d93d91e`,
    /// a git forge = millions of loose objects) at EVERY pod start, restarting
    /// the walk on each ReplicaSet roll. `OnRootMismatch` stats the volume root
    /// instead, so a volume mounted before costs milliseconds.
    #[test]
    fn declaring_an_fs_group_defaults_to_skipping_the_recursive_chown() {
        let structured = CrPodSc {
            fs_group: Some(1000),
            ..Default::default()
        };
        // The fsGroup may arrive on EITHER field; both must default the policy.
        for (label, got) in [
            ("legacy fsGroup", pod_security_context(None, Some(1000))),
            (
                "structured securityContext.fsGroup",
                pod_security_context(Some(&structured), None),
            ),
        ] {
            let got = got.expect("fsGroup must render");
            assert_eq!(got.fs_group, Some(1000), "{label}");
            assert_eq!(
                got.fs_group_change_policy.as_deref(),
                Some("OnRootMismatch"),
                "{label}: must default to OnRootMismatch — `Always` re-walks the \
                 whole volume at every pod start"
            );
        }
    }

    /// The default is a default, not a policy: a CR that says `Always` out loud
    /// gets `Always` (the volume whose ownership must be re-repaired every boot).
    #[test]
    fn pod_security_context_honors_an_explicit_always() {
        let structured: CrPodSc = serde_json::from_value(serde_json::json!({
            "fsGroup": 1001,
            "fsGroupChangePolicy": "Always",
        }))
        .expect("deserialize");
        let got = pod_security_context(Some(&structured), None).expect("must render");
        assert_eq!(
            got.fs_group_change_policy.as_deref(),
            Some("Always"),
            "an explicit fsGroupChangePolicy must survive the fold, never be overwritten"
        );
    }

    /// No fsGroup ⇒ the kubelet never chowns, so a policy would be dead weight.
    /// A hardening-only context (runAsNonRoot/seccomp) stays byte-identical.
    #[test]
    fn pod_security_context_without_an_fs_group_carries_no_policy() {
        let structured = CrPodSc {
            run_as_non_root: Some(true),
            run_as_user: Some(65532),
            ..Default::default()
        };
        let got = pod_security_context(Some(&structured), None).expect("must render");
        assert_eq!(got.fs_group, None);
        assert_eq!(
            got.fs_group_change_policy, None,
            "with no fsGroup there is no chown to skip — emit no policy"
        );
    }

    /// A structured context carries its fields; a legacy fsGroup folds in ONLY
    /// when the structured context omits fsGroup (the nchain/enso shape).
    #[test]
    fn pod_security_context_folds_legacy_fs_group_when_structured_omits_it() {
        let structured = CrPodSc {
            run_as_non_root: Some(true),
            run_as_user: Some(65532),
            seccomp_profile: Some(CrSeccomp {
                type_: "RuntimeDefault".into(),
                localhost_profile: String::new(),
            }),
            ..Default::default()
        };
        let got = pod_security_context(Some(&structured), Some(1001)).expect("must render");
        assert_eq!(got.run_as_non_root, Some(true));
        assert_eq!(got.run_as_user, Some(65532));
        assert_eq!(got.fs_group, Some(1001), "legacy fsGroup folds in");
        assert_eq!(got.seccomp_profile.unwrap().type_, "RuntimeDefault");
    }

    /// The structured fsGroup wins over the legacy top-level field when both set.
    #[test]
    fn pod_security_context_structured_fs_group_wins_over_legacy() {
        let structured = CrPodSc {
            fs_group: Some(2000),
            ..Default::default()
        };
        let got = pod_security_context(Some(&structured), Some(1001)).expect("must render");
        assert_eq!(
            got.fs_group,
            Some(2000),
            "structured fsGroup must win over the legacy top-level field"
        );
    }

    #[test]
    fn topology_spread_is_soft_hostname_self_selecting_for_multi_replica() {
        let mut sel = BTreeMap::new();
        sel.insert("app.kubernetes.io/name".to_string(), "world".to_string());
        sel.insert(
            "app.kubernetes.io/instance".to_string(),
            "world".to_string(),
        );

        let tsc = default_topology_spread(Some(2), &sel).expect("replicas>1 must spread");
        assert_eq!(tsc.len(), 1);
        let c = &tsc[0];
        assert_eq!(c.max_skew, 1, "maxSkew 1 = even spread");
        assert_eq!(
            c.topology_key, "kubernetes.io/hostname",
            "spread across nodes"
        );
        assert_eq!(
            c.when_unsatisfiable, "ScheduleAnyway",
            "SOFT — availability never traded for spread"
        );
        assert_eq!(
            c.label_selector
                .as_ref()
                .and_then(|s| s.match_labels.clone()),
            Some(sel),
            "each app spreads only against its OWN pods"
        );
    }

    #[test]
    fn topology_spread_absent_for_singleton_keeps_deployment_identical() {
        let sel = BTreeMap::new();
        assert!(default_topology_spread(Some(1), &sel).is_none());
        assert!(default_topology_spread(Some(0), &sel).is_none());
        assert!(default_topology_spread(None, &sel).is_none());
    }

    #[test]
    fn image_tag_reads_the_tag_and_ignores_a_registry_port() {
        assert_eq!(image_tag("ghcr.io/hanzoai/cloud:v1.801.233"), Some("v1.801.233"));
        assert_eq!(image_tag("localhost:5000/hanzoai/cloud:v1.2.3"), Some("v1.2.3"));
        // A registry port with no tag must not be mistaken for one.
        assert_eq!(image_tag("localhost:5000/hanzoai/cloud"), None);
        assert_eq!(image_tag("ghcr.io/hanzoai/cloud"), None);
        assert_eq!(image_tag("ghcr.io/hanzoai/cloud:"), None);
    }

    #[test]
    fn image_tag_declines_a_digest_pin() {
        assert_eq!(
            image_tag("ghcr.io/hanzoai/cloud@sha256:5f2b8c1d9e4a7b3c6d8e0f1a2b3c4d5e"),
            None
        );
    }

    #[test]
    fn version_env_is_injected_from_the_image_tag() {
        let env = with_version_env(vec![], "ghcr.io/hanzoai/cloud:v1.801.233");
        let v = env.iter().find(|e| e.name == "HANZO_VERSION").expect("injected");
        assert_eq!(v.value.as_deref(), Some("v1.801.233"));
    }

    #[test]
    fn version_env_never_overwrites_an_explicit_one() {
        let explicit = vec![EnvVar {
            name: "HANZO_VERSION".to_string(),
            value: Some("pinned-by-author".to_string()),
            ..Default::default()
        }];
        let env = with_version_env(explicit, "ghcr.io/hanzoai/cloud:v1.801.233");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].value.as_deref(), Some("pinned-by-author"));
    }

    #[test]
    fn version_env_absent_when_the_image_carries_no_tag() {
        assert!(with_version_env(vec![], "ghcr.io/hanzoai/cloud").is_empty());
    }
}
