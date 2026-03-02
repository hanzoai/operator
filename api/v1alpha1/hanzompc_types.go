package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// MPCDashboardSpec configures the optional MPC dashboard.
type MPCDashboardSpec struct {
	// Enabled controls whether the dashboard is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the dashboard container image.
	// +kubebuilder:default="ghcr.io/hanzoai/mpc-dashboard:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// Resources configures CPU and memory for the dashboard.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// MPCCacheSpec configures the optional Valkey cache.
type MPCCacheSpec struct {
	// Enabled controls whether the cache is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the Valkey container image.
	// +kubebuilder:default="ghcr.io/hanzoai/kv:8"
	// +optional
	Image string `json:"image,omitempty"`

	// Resources configures CPU and memory for the cache.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// HanzoMPCSpec defines the desired state of HanzoMPC.
type HanzoMPCSpec struct {
	// Image defines the MPC node container image.
	// +kubebuilder:validation:Required
	Image ImageSpec `json:"image"`

	// Replicas is the number of MPC nodes in the cluster.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Minimum=2
	Replicas int32 `json:"replicas"`

	// Threshold is the minimum number of nodes required for signing.
	// Must satisfy: replicas >= threshold + 1.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Minimum=1
	Threshold int32 `json:"threshold"`

	// P2PPort is the port used for inter-node communication.
	// +kubebuilder:default=4000
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	P2PPort int32 `json:"p2pPort,omitempty"`

	// APIPort is the port exposed for API access on pod-0.
	// +kubebuilder:default=8080
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	APIPort int32 `json:"apiPort,omitempty"`

	// Resources configures CPU and memory requests/limits.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// Dashboard configures the optional MPC dashboard.
	// +optional
	Dashboard *MPCDashboardSpec `json:"dashboard,omitempty"`

	// Cache configures the optional Valkey cache.
	// +optional
	Cache *MPCCacheSpec `json:"cache,omitempty"`

	// Ingress configures external access via an Ingress resource.
	// +optional
	Ingress *IngressSpec `json:"ingress,omitempty"`

	// Labels are additional labels applied to all managed resources.
	// +optional
	Labels map[string]string `json:"labels,omitempty"`

	// Annotations are additional annotations applied to all managed resources.
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`

	// ImagePullSecrets lists references to secrets for pulling container images.
	// +optional
	ImagePullSecrets []string `json:"imagePullSecrets,omitempty"`
}

// HanzoMPCStatus defines the observed state of HanzoMPC.
type HanzoMPCStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyNodes is the number of MPC nodes that are ready.
	// +optional
	ReadyNodes int32 `json:"readyNodes,omitempty"`

	// KeysGenerated indicates whether the MPC key shares have been generated.
	// +optional
	KeysGenerated bool `json:"keysGenerated,omitempty"`

	// Conditions represent the latest observations of the MPC cluster's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hmpc
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyNodes`
// +kubebuilder:printcolumn:name="Threshold",type=integer,JSONPath=`.spec.threshold`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoMPC is the Schema for the hanzompcs API.
// It manages a multi-party computation threshold signing cluster.
type HanzoMPC struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoMPCSpec   `json:"spec,omitempty"`
	Status HanzoMPCStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoMPCList contains a list of HanzoMPC.
type HanzoMPCList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoMPC `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoMPC{}, &HanzoMPCList{})
}
