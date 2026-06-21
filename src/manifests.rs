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
    ConfigMap, Container, ContainerPort, EnvFromSource, EnvVar, ExecAction, HTTPGetAction,
    Lifecycle, LifecycleHandler, LocalObjectReference, PersistentVolumeClaim, PodSpec,
    PodTemplateSpec, Probe, ResourceRequirements as K8sResourceRequirements,
    Service as CoreService, ServicePort, ServiceSpec as CoreServiceSpec, Volume, VolumeMount,
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
    ResourceRequirements, ServicePort as CrServicePort,
};

pub const LABEL_NAME: &str = "app.kubernetes.io/name";
pub const LABEL_INSTANCE: &str = "app.kubernetes.io/instance";
pub const LABEL_COMPONENT: &str = "app.kubernetes.io/component";
pub const LABEL_PART_OF: &str = "app.kubernetes.io/part-of";
pub const LABEL_VERSION: &str = "app.kubernetes.io/version";
pub const LABEL_MANAGED_BY: &str = "app.kubernetes.io/managed-by";
pub const MANAGED_BY_VALUE: &str = "hanzo-operator";

/// Build the standard `app.kubernetes.io/*` label set. Empty values omitted.
pub fn standard_labels(
    name: &str,
    component: &str,
    part_of: &str,
    version: &str,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_NAME.to_string(), name.to_string());
    labels.insert(LABEL_INSTANCE.to_string(), name.to_string());
    labels.insert(LABEL_MANAGED_BY.to_string(), MANAGED_BY_VALUE.to_string());
    if !component.is_empty() {
        labels.insert(LABEL_COMPONENT.to_string(), component.to_string());
    }
    if !part_of.is_empty() {
        labels.insert(LABEL_PART_OF.to_string(), part_of.to_string());
    }
    if !version.is_empty() {
        labels.insert(LABEL_VERSION.to_string(), version.to_string());
    }
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
                    termination_grace_period_seconds: Some(30),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
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
            service_name: service_name.to_string(),
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
    // hanzoai/ingress (Traefik fork) silently drops spec.tls when the caller
    // sets spec.ingressClassName instead of the annotation. Emit the
    // annotation form so TLS stays hooked up.
    if !spec.ingress_class_name.is_empty() {
        annotations.insert(
            "kubernetes.io/ingress.class".to_string(),
            spec.ingress_class_name.clone(),
        );
    }

    let path_type = "Prefix".to_string();
    let mut rules = Vec::new();
    for host in &spec.hosts {
        let mut paths = vec![HTTPIngressPath {
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
        }];

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
            pod_selector: LabelSelector {
                match_labels: Some(selector_labels_map),
                ..Default::default()
            },
            policy_types: Some(vec!["Ingress".to_string()]),
            ingress,
            ..Default::default()
        }),
    }
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
    use k8s_openapi::api::core::v1::{PersistentVolumeClaimSpec, ResourceRequirements as K8sRR};
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
            resources: Some(K8sRR {
                requests: Some({
                    let mut m = BTreeMap::new();
                    m.insert("storage".to_string(), Quantity(size.to_string()));
                    m
                }),
                limits: None,
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}
