package manifests

import (
	appsv1 "k8s.io/api/apps/v1"
	autoscalingv2 "k8s.io/api/autoscaling/v2"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/util/intstr"

	v1alpha1 "github.com/hanzoai/operator/api/v1alpha1"
)

// BuildDeployment creates a skeleton Deployment.
func BuildDeployment(
	name, namespace string,
	labels, selectorLabels map[string]string,
	replicas *int32,
	containers []corev1.Container,
	volumes []corev1.Volume,
	strategy v1alpha1.DeploymentStrategy,
	imagePullSecrets []corev1.LocalObjectReference,
	serviceAccountName string,
) *appsv1.Deployment {
	s := appsv1.DeploymentStrategy{
		Type: appsv1.RollingUpdateDeploymentStrategyType,
		RollingUpdate: &appsv1.RollingUpdateDeployment{
			MaxSurge:       intStrPtr(1),
			MaxUnavailable: intStrPtr(0),
		},
	}
	if strategy == v1alpha1.DeploymentStrategyRecreate {
		s = appsv1.DeploymentStrategy{
			Type: appsv1.RecreateDeploymentStrategyType,
		}
	}

	terminationGrace := int64(30)

	// Inject preStop lifecycle hook for graceful shutdown.
	containers = injectPreStopHook(containers)

	return &appsv1.Deployment{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    labels,
		},
		Spec: appsv1.DeploymentSpec{
			Replicas:        replicas,
			MinReadySeconds: 10,
			Selector: &metav1.LabelSelector{
				MatchLabels: selectorLabels,
			},
			Strategy: s,
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{
					Labels: labels,
				},
				Spec: corev1.PodSpec{
					Containers:                    containers,
					Volumes:                       volumes,
					ImagePullSecrets:              imagePullSecrets,
					ServiceAccountName:            serviceAccountName,
					TerminationGracePeriodSeconds: &terminationGrace,
				},
			},
		},
	}
}

// BuildStatefulSet creates a skeleton StatefulSet.
func BuildStatefulSet(
	name, namespace string,
	labels, selectorLabels map[string]string,
	replicas *int32,
	containers []corev1.Container,
	volumes []corev1.Volume,
	pvcTemplates []corev1.PersistentVolumeClaim,
	imagePullSecrets []corev1.LocalObjectReference,
	serviceName string,
) *appsv1.StatefulSet {
	terminationGrace := int64(30)

	// Inject preStop lifecycle hook for graceful shutdown.
	containers = injectPreStopHook(containers)

	return &appsv1.StatefulSet{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    labels,
		},
		Spec: appsv1.StatefulSetSpec{
			Replicas:        replicas,
			MinReadySeconds: 10,
			ServiceName:     serviceName,
			UpdateStrategy: appsv1.StatefulSetUpdateStrategy{
				Type: appsv1.RollingUpdateStatefulSetStrategyType,
			},
			Selector: &metav1.LabelSelector{
				MatchLabels: selectorLabels,
			},
			VolumeClaimTemplates: pvcTemplates,
			Template: corev1.PodTemplateSpec{
				ObjectMeta: metav1.ObjectMeta{
					Labels: labels,
				},
				Spec: corev1.PodSpec{
					Containers:                    containers,
					Volumes:                       volumes,
					ImagePullSecrets:              imagePullSecrets,
					TerminationGracePeriodSeconds: &terminationGrace,
				},
			},
		},
	}
}

// BuildService creates a ClusterIP Service.
func BuildService(
	name, namespace string,
	labels map[string]string,
	ports []corev1.ServicePort,
	selectorLabels map[string]string,
) *corev1.Service {
	return &corev1.Service{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    labels,
		},
		Spec: corev1.ServiceSpec{
			Type:     corev1.ServiceTypeClusterIP,
			Selector: selectorLabels,
			Ports:    ports,
		},
	}
}

// BuildHeadlessService creates a headless Service (ClusterIP: None) for
// StatefulSet DNS.
func BuildHeadlessService(
	name, namespace string,
	labels map[string]string,
	ports []corev1.ServicePort,
	selectorLabels map[string]string,
) *corev1.Service {
	svc := BuildService(name, namespace, labels, ports, selectorLabels)
	svc.Spec.ClusterIP = "None"
	return svc
}

// BuildIngress creates an Ingress resource with cert-manager annotations.
func BuildIngress(
	name, namespace string,
	spec *v1alpha1.IngressSpec,
	serviceName string,
	servicePort int32,
	labels map[string]string,
) *networkingv1.Ingress {
	pathType := networkingv1.PathTypePrefix
	annotations := map[string]string{}

	if spec.TLS {
		annotations["cert-manager.io/cluster-issuer"] = spec.ClusterIssuer
	}
	for k, v := range spec.Annotations {
		annotations[k] = v
	}

	var rules []networkingv1.IngressRule
	for _, host := range spec.Hosts {
		paths := []networkingv1.HTTPIngressPath{
			{
				Path:     "/",
				PathType: &pathType,
				Backend: networkingv1.IngressBackend{
					Service: &networkingv1.IngressServiceBackend{
						Name: serviceName,
						Port: networkingv1.ServiceBackendPort{
							Number: servicePort,
						},
					},
				},
			},
		}

		// Add custom path rules.
		for _, pr := range spec.PathRules {
			pt := pathTypeFromString(pr.PathType)
			backendSvc := serviceName
			if pr.ServiceName != "" {
				backendSvc = pr.ServiceName
			}
			paths = append(paths, networkingv1.HTTPIngressPath{
				Path:     pr.Path,
				PathType: &pt,
				Backend: networkingv1.IngressBackend{
					Service: &networkingv1.IngressServiceBackend{
						Name: backendSvc,
						Port: networkingv1.ServiceBackendPort{
							Number: pr.Port,
						},
					},
				},
			})
		}

		rules = append(rules, networkingv1.IngressRule{
			Host: host,
			IngressRuleValue: networkingv1.IngressRuleValue{
				HTTP: &networkingv1.HTTPIngressRuleValue{
					Paths: paths,
				},
			},
		})
	}

	var tls []networkingv1.IngressTLS
	if spec.TLS && len(spec.Hosts) > 0 {
		tls = append(tls, networkingv1.IngressTLS{
			Hosts:      spec.Hosts,
			SecretName: name + "-tls",
		})
	}

	ing := &networkingv1.Ingress{
		ObjectMeta: metav1.ObjectMeta{
			Name:        name,
			Namespace:   namespace,
			Labels:      labels,
			Annotations: annotations,
		},
		Spec: networkingv1.IngressSpec{
			Rules: rules,
			TLS:   tls,
		},
	}

	if spec.IngressClassName != "" {
		ing.Spec.IngressClassName = &spec.IngressClassName
	}

	return ing
}

// BuildHPA creates a HorizontalPodAutoscaler targeting a Deployment.
func BuildHPA(
	name, namespace string,
	targetRef autoscalingv2.CrossVersionObjectReference,
	spec *v1alpha1.AutoscalingSpec,
	labels map[string]string,
) *autoscalingv2.HorizontalPodAutoscaler {
	var metrics []autoscalingv2.MetricSpec

	if spec.TargetCPUUtilization != nil {
		metrics = append(metrics, autoscalingv2.MetricSpec{
			Type: autoscalingv2.ResourceMetricSourceType,
			Resource: &autoscalingv2.ResourceMetricSource{
				Name: corev1.ResourceCPU,
				Target: autoscalingv2.MetricTarget{
					Type:               autoscalingv2.UtilizationMetricType,
					AverageUtilization: spec.TargetCPUUtilization,
				},
			},
		})
	}

	if spec.TargetMemoryUtilization != nil {
		metrics = append(metrics, autoscalingv2.MetricSpec{
			Type: autoscalingv2.ResourceMetricSourceType,
			Resource: &autoscalingv2.ResourceMetricSource{
				Name: corev1.ResourceMemory,
				Target: autoscalingv2.MetricTarget{
					Type:               autoscalingv2.UtilizationMetricType,
					AverageUtilization: spec.TargetMemoryUtilization,
				},
			},
		})
	}

	return &autoscalingv2.HorizontalPodAutoscaler{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    labels,
		},
		Spec: autoscalingv2.HorizontalPodAutoscalerSpec{
			ScaleTargetRef: targetRef,
			MinReplicas:    spec.MinReplicas,
			MaxReplicas:    derefInt32(spec.MaxReplicas, 10),
			Metrics:        metrics,
		},
	}
}

// BuildPDB creates a PodDisruptionBudget.
func BuildPDB(
	name, namespace string,
	spec *v1alpha1.PodDisruptionBudgetSpec,
	selectorLabels map[string]string,
	labels map[string]string,
) *policyv1.PodDisruptionBudget {
	pdb := &policyv1.PodDisruptionBudget{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    labels,
		},
		Spec: policyv1.PodDisruptionBudgetSpec{
			Selector: &metav1.LabelSelector{
				MatchLabels: selectorLabels,
			},
		},
	}

	if spec.MinAvailable != nil {
		val := intstr.FromInt32(*spec.MinAvailable)
		pdb.Spec.MinAvailable = &val
	} else if spec.MaxUnavailable != nil {
		val := intstr.FromInt32(*spec.MaxUnavailable)
		pdb.Spec.MaxUnavailable = &val
	} else {
		// Default: at least 1 available.
		val := intstr.FromInt32(1)
		pdb.Spec.MinAvailable = &val
	}

	return pdb
}

// BuildNetworkPolicy creates a NetworkPolicy for the given pod selector.
func BuildNetworkPolicy(
	name, namespace string,
	spec *v1alpha1.NetworkPolicySpec,
	selectorLabels map[string]string,
	labels map[string]string,
) *networkingv1.NetworkPolicy {
	np := &networkingv1.NetworkPolicy{
		ObjectMeta: metav1.ObjectMeta{
			Name:      name,
			Namespace: namespace,
			Labels:    labels,
		},
		Spec: networkingv1.NetworkPolicySpec{
			PodSelector: metav1.LabelSelector{
				MatchLabels: selectorLabels,
			},
			PolicyTypes: []networkingv1.PolicyType{
				networkingv1.PolicyTypeIngress,
			},
		},
	}

	var from []networkingv1.NetworkPolicyPeer

	// Allow intra-namespace traffic by default.
	if spec.AllowIntraNamespace == nil || *spec.AllowIntraNamespace {
		from = append(from, networkingv1.NetworkPolicyPeer{
			PodSelector: &metav1.LabelSelector{},
		})
	}

	// Additional peers.
	for _, peer := range spec.AllowFrom {
		npp := networkingv1.NetworkPolicyPeer{}
		if peer.PodSelector != nil {
			npp.PodSelector = peer.PodSelector
		}
		if peer.NamespaceSelector != nil {
			npp.NamespaceSelector = peer.NamespaceSelector
		}
		from = append(from, npp)
	}

	if spec.AllowIngress {
		// Empty from list means allow all ingress.
		np.Spec.Ingress = []networkingv1.NetworkPolicyIngressRule{{}}
	} else if len(from) > 0 {
		np.Spec.Ingress = []networkingv1.NetworkPolicyIngressRule{
			{From: from},
		}
	}

	return np
}

func pathTypeFromString(s string) networkingv1.PathType {
	switch s {
	case "Exact":
		return networkingv1.PathTypeExact
	case "ImplementationSpecific":
		return networkingv1.PathTypeImplementationSpecific
	default:
		return networkingv1.PathTypePrefix
	}
}

func derefInt32(p *int32, def int32) int32 {
	if p != nil {
		return *p
	}
	return def
}

// intStrPtr returns a pointer to an IntOrString from an int value.
func intStrPtr(val int32) *intstr.IntOrString {
	v := intstr.FromInt32(val)
	return &v
}

// injectPreStopHook adds a preStop lifecycle hook to each container that
// does not already have one. The hook sleeps for 5 seconds to allow
// in-flight requests to drain before the container receives SIGTERM.
func injectPreStopHook(containers []corev1.Container) []corev1.Container {
	out := make([]corev1.Container, len(containers))
	for i, c := range containers {
		if c.Lifecycle == nil {
			c.Lifecycle = &corev1.Lifecycle{}
		}
		if c.Lifecycle.PreStop == nil {
			c.Lifecycle.PreStop = &corev1.LifecycleHandler{
				Exec: &corev1.ExecAction{
					Command: []string{"/bin/sh", "-c", "sleep 5"},
				},
			}
		}
		out[i] = c
	}
	return out
}
