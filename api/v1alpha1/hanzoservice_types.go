package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// DeploymentStrategy controls how pods are replaced during updates.
// +kubebuilder:validation:Enum=RollingUpdate;Recreate
type DeploymentStrategy string

const (
	DeploymentStrategyRollingUpdate DeploymentStrategy = "RollingUpdate"
	DeploymentStrategyRecreate     DeploymentStrategy = "Recreate"
)

// ServicePort defines a port exposed by a HanzoService.
type ServicePort struct {
	// Name is a human-readable label for this port.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// ContainerPort is the port the container listens on.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	ContainerPort int32 `json:"containerPort"`

	// ServicePort is the port exposed on the Kubernetes Service.
	// Defaults to ContainerPort if unset.
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	ServicePort *int32 `json:"servicePort,omitempty"`

	// Protocol is the network protocol for this port.
	// +kubebuilder:default="TCP"
	// +kubebuilder:validation:Enum=TCP;UDP;SCTP
	// +optional
	Protocol corev1.Protocol `json:"protocol,omitempty"`
}

// HanzoServiceSpec defines the desired state of HanzoService.
type HanzoServiceSpec struct {
	// Image defines the container image to deploy.
	// +kubebuilder:validation:Required
	Image ImageSpec `json:"image"`

	// Replicas is the desired number of pod replicas.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=0
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Ports lists the ports exposed by this service.
	// +optional
	Ports []ServicePort `json:"ports,omitempty"`

	// Env is a list of environment variables to set in the container.
	// +optional
	Env []corev1.EnvVar `json:"env,omitempty"`

	// EnvFrom is a list of sources to populate environment variables.
	// +optional
	EnvFrom []corev1.EnvFromSource `json:"envFrom,omitempty"`

	// Resources configures CPU and memory requests/limits.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// LivenessProbe configures the container liveness check.
	// +optional
	LivenessProbe *ProbeSpec `json:"livenessProbe,omitempty"`

	// ReadinessProbe configures the container readiness check.
	// +optional
	ReadinessProbe *ProbeSpec `json:"readinessProbe,omitempty"`

	// Ingress configures external access via an Ingress resource.
	// +optional
	Ingress *IngressSpec `json:"ingress,omitempty"`

	// Autoscaling configures horizontal pod autoscaling.
	// +optional
	Autoscaling *AutoscalingSpec `json:"autoscaling,omitempty"`

	// PDB configures a PodDisruptionBudget.
	// +optional
	PDB *PodDisruptionBudgetSpec `json:"pdb,omitempty"`

	// NetworkPolicy configures network-level access control.
	// +optional
	NetworkPolicy *NetworkPolicySpec `json:"networkPolicy,omitempty"`

	// ServiceMonitor configures Prometheus metrics scraping.
	// +optional
	ServiceMonitor *ServiceMonitorSpec `json:"serviceMonitor,omitempty"`

	// KMSSecrets lists KMS secret references to sync.
	// +optional
	KMSSecrets []KMSSecretRef `json:"kmsSecrets,omitempty"`

	// Sidecars are additional containers to run alongside the main container.
	// +optional
	Sidecars []corev1.Container `json:"sidecars,omitempty"`

	// ImagePullSecrets lists references to secrets for pulling container images.
	// +optional
	ImagePullSecrets []corev1.LocalObjectReference `json:"imagePullSecrets,omitempty"`

	// ServiceAccountName is the name of the ServiceAccount to run pods as.
	// +optional
	ServiceAccountName string `json:"serviceAccountName,omitempty"`

	// Strategy controls how pods are replaced during updates.
	// +kubebuilder:default="RollingUpdate"
	// +optional
	Strategy DeploymentStrategy `json:"strategy,omitempty"`

	// Labels are additional labels applied to all managed resources.
	// +optional
	Labels map[string]string `json:"labels,omitempty"`

	// Annotations are additional annotations applied to all managed resources.
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`

	// PartOf identifies the application this service belongs to (app.kubernetes.io/part-of).
	// +optional
	PartOf string `json:"partOf,omitempty"`

	// Component identifies the component within the application (app.kubernetes.io/component).
	// +optional
	Component string `json:"component,omitempty"`

	// Volumes defines additional volumes to attach to the pod.
	// +optional
	Volumes []corev1.Volume `json:"volumes,omitempty"`

	// VolumeMounts defines additional volume mounts for the main container.
	// +optional
	VolumeMounts []corev1.VolumeMount `json:"volumeMounts,omitempty"`
}

// HanzoServiceStatus defines the observed state of HanzoService.
type HanzoServiceStatus struct {
	// Phase is the current lifecycle phase of the service.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyReplicas is the number of replicas that have passed readiness checks.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// AvailableReplicas is the number of replicas available to serve traffic.
	// +optional
	AvailableReplicas int32 `json:"availableReplicas,omitempty"`

	// Conditions represent the latest observations of the service's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`

	// Endpoints lists the URLs where this service is reachable.
	// +optional
	Endpoints []string `json:"endpoints,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hsvc
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Image",type=string,JSONPath=`.spec.image.repository`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoService is the Schema for the hanzoservices API.
// It models stateless services such as chat, cloud, api, paas, models, and pricing.
type HanzoService struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoServiceSpec   `json:"spec,omitempty"`
	Status HanzoServiceStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoServiceList contains a list of HanzoService.
type HanzoServiceList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoService `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoService{}, &HanzoServiceList{})
}
