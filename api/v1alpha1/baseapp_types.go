// Copyright 2026 Hanzo AI.
// Licensed under the Apache License, Version 2.0.

package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// BaseAppGatewaySpec binds a BaseApp to a gateway route. When populated,
// the operator patches the target gateway's ConfigMap (or emits a Route
// CR when a HanzoGateway owner exists) with a backend of kind `base_ha`.
type BaseAppGatewaySpec struct {
	// Route is the URL pattern the gateway exposes (e.g. "/v1/app/foo").
	// When empty, no gateway wiring is emitted.
	// +optional
	Route string `json:"route,omitempty"`

	// GatewayName is the ConfigMap name holding the gateway config. If
	// empty, defaults to "gateway" in the HanzoApp namespace.
	// +optional
	GatewayName string `json:"gatewayName,omitempty"`

	// GatewayNamespace is the namespace of the gateway ConfigMap. If
	// empty, defaults to the BaseApp namespace.
	// +optional
	GatewayNamespace string `json:"gatewayNamespace,omitempty"`

	// LeaderPollInterval is forwarded to the gateway base_ha upstream.
	// Minimum 100ms; default 1s.
	// +kubebuilder:default="1s"
	// +optional
	LeaderPollInterval string `json:"leaderPollInterval,omitempty"`

	// ReadYourWritesTTL is forwarded to the gateway base_ha upstream.
	// Set to "0s" to disable read-your-writes pinning.
	// +kubebuilder:default="5s"
	// +optional
	ReadYourWritesTTL string `json:"readYourWritesTTL,omitempty"`
}

// BaseAppSpec defines the desired state of a BaseApp — a hanzoai/base-ha
// cluster managed declaratively. The operator emits a StatefulSet, a
// headless service (for pod DNS), a ClusterIP service (for gateway
// round-robin reads), a NetworkPolicy, and (optionally) gateway routing.
type BaseAppSpec struct {
	// Image defines the container image for the base-ha binary.
	// +kubebuilder:validation:Required
	Image ImageSpec `json:"image"`

	// Replicas is the desired StatefulSet size. Writer election is
	// deterministic (lowest-sorted alive pod), so any odd number >= 3
	// tolerates floor(n/2) failures.
	// +kubebuilder:default=3
	// +kubebuilder:validation:Minimum=1
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Port is the HTTP port the base-ha pod listens on. Defaults to 8090.
	// +kubebuilder:default=8090
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	Port int32 `json:"port,omitempty"`

	// Consensus selects the writer-pin protocol.
	// +kubebuilder:default="quasar"
	// +kubebuilder:validation:Enum=quasar;pubsub
	// +optional
	Consensus string `json:"consensus,omitempty"`

	// Schema names a ConfigMap whose contents are mounted at /app/schema
	// so the base-ha binary can pick up collection migrations on boot.
	// The ConfigMap is expected to contain one Base migration file per
	// key. When empty, the image's built-in schema is used.
	// +optional
	Schema string `json:"schema,omitempty"`

	// Storage configures the per-pod persistent volume claim template.
	// +kubebuilder:validation:Required
	Storage StorageSpec `json:"storage"`

	// Resources configures CPU and memory requests/limits for the
	// base-ha container.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// Env is appended to the generated env (BASE_*) so operators can
	// supply secrets or feature flags.
	// +optional
	Env []corev1.EnvVar `json:"env,omitempty"`

	// EnvFrom is appended to the generated envFrom.
	// +optional
	EnvFrom []corev1.EnvFromSource `json:"envFrom,omitempty"`

	// ImagePullSecrets are forwarded to the StatefulSet.
	// +optional
	ImagePullSecrets []corev1.LocalObjectReference `json:"imagePullSecrets,omitempty"`

	// ServiceAccountName sets the pod identity. Defaults to the
	// namespace-default.
	// +optional
	ServiceAccountName string `json:"serviceAccountName,omitempty"`

	// Gateway configures gateway wiring for this BaseApp. Optional — a
	// BaseApp without gateway config is still reachable via its ClusterIP
	// service directly.
	// +optional
	Gateway *BaseAppGatewaySpec `json:"gateway,omitempty"`

	// IAMApp identifies the optional Hanzo IAM application that owns
	// authentication for this app. When set, the operator forwards the
	// IAM_APP env var so base-ha's boot-time KMS fetch can resolve secrets.
	// +optional
	IAMApp string `json:"iamApp,omitempty"`

	// KMSSecrets lists KMS secret references synced into K8s Secrets
	// alongside the StatefulSet. Uses the same kms.hanzo.ai/v1alpha1
	// KMSSecret CRD as HanzoService / HanzoDatastore.
	// +optional
	KMSSecrets []KMSSecretRef `json:"kmsSecrets,omitempty"`

	// NetworkPolicy configures intra-cluster ingress rules. When nil,
	// the operator emits a default policy allowing pod-to-pod traffic
	// on the BASE port and the HA replication endpoints, plus gateway
	// ingress.
	// +optional
	NetworkPolicy *NetworkPolicySpec `json:"networkPolicy,omitempty"`

	// ServiceMonitor configures Prometheus scraping of base-ha pod
	// metrics.
	// +optional
	ServiceMonitor *ServiceMonitorSpec `json:"serviceMonitor,omitempty"`

	// PartOf identifies the application this BaseApp belongs to
	// (app.kubernetes.io/part-of).
	// +optional
	PartOf string `json:"partOf,omitempty"`
}

// BaseAppStatus reports observed state.
type BaseAppStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyReplicas is the number of pods that have passed readiness.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// CurrentWriter is the pod DNS name of the last observed writer.
	// Populated from the gateway's leader poll (when a gateway route
	// is configured) or left blank when unknown.
	// +optional
	CurrentWriter string `json:"currentWriter,omitempty"`

	// Term is the monotonic writer-election term observed by the
	// operator. Strictly increasing across failovers.
	// +optional
	Term uint64 `json:"term,omitempty"`

	// Conditions reports detailed state transitions.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=bapp
// +kubebuilder:printcolumn:name="Replicas",type=integer,JSONPath=`.spec.replicas`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Writer",type=string,JSONPath=`.status.currentWriter`
// +kubebuilder:printcolumn:name="Term",type=integer,JSONPath=`.status.term`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// BaseApp is the Schema for the baseapps API. It models a
// hanzoai/base-ha cluster: N replicas, one writer pinned by Quasar,
// gateway-integrated via the base_ha upstream.
type BaseApp struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   BaseAppSpec   `json:"spec,omitempty"`
	Status BaseAppStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// BaseAppList contains a list of BaseApp.
type BaseAppList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []BaseApp `json:"items"`
}

func init() {
	SchemeBuilder.Register(&BaseApp{}, &BaseAppList{})
}

// Compile-time check that resource.Quantity import is kept for StorageSpec.
var _ = resource.Quantity{}
