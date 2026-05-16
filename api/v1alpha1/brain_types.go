// Copyright 2026 Hanzo AI.
// Licensed under the Apache License, Version 2.0.

package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// BrainStorageSpec configures the per-tenant brain persistent volume.
type BrainStorageSpec struct {
	// Size is the requested storage capacity (e.g. "10Gi").
	// +kubebuilder:default="10Gi"
	// +optional
	Size string `json:"size,omitempty"`

	// StorageClassName names the StorageClass. Empty uses cluster default.
	// +optional
	StorageClassName string `json:"storageClassName,omitempty"`
}

// BrainSpec defines the desired state of a Brain — the per-tenant memory
// and recipe store for hanzoai/brain. One Brain per org. The operator
// emits a StatefulSet, a headless Service, a ClusterIP Service, and a
// PVC template.
type BrainSpec struct {
	// Image defines the container image for the brain binary.
	// +kubebuilder:validation:Required
	Image ImageSpec `json:"image"`

	// Replicas is the desired StatefulSet size. Defaults to 1.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=0
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Host is the external hostname exposed via the org gateway.
	// +optional
	Host string `json:"host,omitempty"`

	// Storage configures the per-pod persistent volume claim template.
	// +kubebuilder:validation:Required
	Storage BrainStorageSpec `json:"storage"`

	// Resources configures CPU and memory requests/limits.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// BrainStatus defines the observed state of Brain.
type BrainStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyReplicas is the number of replicas that have passed readiness checks.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// Conditions represent the latest observations of the brain's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`

	// Endpoint is the in-cluster URL where this brain is reachable.
	// +optional
	Endpoint string `json:"endpoint,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=brain
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Image",type=string,JSONPath=`.spec.image.repository`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// Brain is the per-tenant memory and recipe store for Hanzo Agents.
type Brain struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   BrainSpec   `json:"spec,omitempty"`
	Status BrainStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// BrainList contains a list of Brain.
type BrainList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Brain `json:"items"`
}

func init() {
	SchemeBuilder.Register(&Brain{}, &BrainList{})
}
