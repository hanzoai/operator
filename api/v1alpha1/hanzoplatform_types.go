package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// PlatformServiceSpec is an inline spec for creating a child HanzoService.
type PlatformServiceSpec struct {
	// Name is the name for the child HanzoService CR.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Spec is the HanzoService spec.
	// +kubebuilder:validation:Required
	Spec HanzoServiceSpec `json:"spec"`
}

// PlatformDatastoreSpec is an inline spec for creating a child HanzoDatastore.
type PlatformDatastoreSpec struct {
	// Name is the name for the child HanzoDatastore CR.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Spec is the HanzoDatastore spec.
	// +kubebuilder:validation:Required
	Spec HanzoDatastoreSpec `json:"spec"`
}

// PlatformGatewaySpec is an inline spec for creating a child HanzoGateway.
type PlatformGatewaySpec struct {
	// Name is the name for the child HanzoGateway CR.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Spec is the HanzoGateway spec.
	// +kubebuilder:validation:Required
	Spec HanzoGatewaySpec `json:"spec"`
}

// PlatformMPCSpec is an inline spec for creating a child HanzoMPC.
type PlatformMPCSpec struct {
	// Name is the name for the child HanzoMPC CR.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Spec is the HanzoMPC spec.
	// +kubebuilder:validation:Required
	Spec HanzoMPCSpec `json:"spec"`
}

// PlatformNetworkSpec is an inline spec for creating a child HanzoNetwork.
type PlatformNetworkSpec struct {
	// Name is the name for the child HanzoNetwork CR.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Spec is the HanzoNetwork spec.
	// +kubebuilder:validation:Required
	Spec HanzoNetworkSpec `json:"spec"`
}

// PlatformIngressSpec is an inline spec for creating a child HanzoIngress.
type PlatformIngressSpec struct {
	// Name is the name for the child HanzoIngress CR.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Spec is the HanzoIngress spec.
	// +kubebuilder:validation:Required
	Spec HanzoIngressSpec `json:"spec"`
}

// PlatformNetworkPolicies configures platform-level network policies.
type PlatformNetworkPolicies struct {
	// DefaultDeny controls whether a default-deny NetworkPolicy is created.
	// +optional
	DefaultDeny bool `json:"defaultDeny,omitempty"`
}

// HanzoPlatformSpec defines the desired state of HanzoPlatform.
type HanzoPlatformSpec struct {
	// Services defines the inline HanzoService specs to create.
	// +optional
	Services []PlatformServiceSpec `json:"services,omitempty"`

	// Datastores defines the inline HanzoDatastore specs to create.
	// +optional
	Datastores []PlatformDatastoreSpec `json:"datastores,omitempty"`

	// Gateways defines the inline HanzoGateway specs to create.
	// +optional
	Gateways []PlatformGatewaySpec `json:"gateways,omitempty"`

	// MPCs defines the inline HanzoMPC specs to create.
	// +optional
	MPCs []PlatformMPCSpec `json:"mpcs,omitempty"`

	// Networks defines the inline HanzoNetwork specs to create.
	// +optional
	Networks []PlatformNetworkSpec `json:"networks,omitempty"`

	// Ingresses defines the inline HanzoIngress specs to create.
	// +optional
	Ingresses []PlatformIngressSpec `json:"ingresses,omitempty"`

	// NetworkPolicies configures platform-level network policies.
	// +optional
	NetworkPolicies *PlatformNetworkPolicies `json:"networkPolicies,omitempty"`

	// Labels are additional labels applied to all child resources.
	// +optional
	Labels map[string]string `json:"labels,omitempty"`
}

// HanzoPlatformStatus defines the observed state of HanzoPlatform.
type HanzoPlatformStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ServiceCount is the number of child HanzoService CRs.
	// +optional
	ServiceCount int32 `json:"serviceCount,omitempty"`

	// DatastoreCount is the number of child HanzoDatastore CRs.
	// +optional
	DatastoreCount int32 `json:"datastoreCount,omitempty"`

	// ReadyServices is the number of child services in Running phase.
	// +optional
	ReadyServices int32 `json:"readyServices,omitempty"`

	// Conditions represent the latest observations of the platform's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hpf
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Services",type=integer,JSONPath=`.status.serviceCount`
// +kubebuilder:printcolumn:name="Datastores",type=integer,JSONPath=`.status.datastoreCount`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyServices`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoPlatform is the Schema for the hanzoplatforms API.
// It is the top-level orchestrator that manages child HanzoService, HanzoDatastore,
// HanzoGateway, HanzoMPC, HanzoNetwork, and HanzoIngress resources.
type HanzoPlatform struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoPlatformSpec   `json:"spec,omitempty"`
	Status HanzoPlatformStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoPlatformList contains a list of HanzoPlatform.
type HanzoPlatformList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoPlatform `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoPlatform{}, &HanzoPlatformList{})
}
