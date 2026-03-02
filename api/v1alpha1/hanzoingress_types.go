package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// IngressRoute defines a route within a domain configuration.
type IngressRoute struct {
	// Path is the URL path to match.
	// +kubebuilder:validation:Required
	Path string `json:"path"`

	// PathType specifies how the path is matched.
	// +kubebuilder:default="Prefix"
	// +kubebuilder:validation:Enum=Exact;Prefix;ImplementationSpecific
	// +optional
	PathType string `json:"pathType,omitempty"`

	// ServiceName is the backend Kubernetes Service name.
	// +kubebuilder:validation:Required
	ServiceName string `json:"serviceName"`

	// ServicePort is the backend service port.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	ServicePort int32 `json:"servicePort"`
}

// DomainConfig defines the routing configuration for a single domain.
type DomainConfig struct {
	// Domain is the fully qualified domain name.
	// +kubebuilder:validation:Required
	Domain string `json:"domain"`

	// Routes defines the routing rules for this domain.
	// +kubebuilder:validation:MinItems=1
	Routes []IngressRoute `json:"routes"`

	// TLS controls whether TLS is enabled for this domain.
	// +kubebuilder:default=true
	// +optional
	TLS bool `json:"tls,omitempty"`
}

// IngressDaemonSetSpec configures a DaemonSet-based ingress controller.
type IngressDaemonSetSpec struct {
	// Enabled controls whether the ingress DaemonSet is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the ingress controller container image.
	// +kubebuilder:default="ghcr.io/hanzoai/ingress:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// CloudflareCredentials references a Secret containing Cloudflare API credentials
	// for ACME DNS-01 challenges.
	// +optional
	CloudflareCredentials *corev1.SecretReference `json:"cloudflareCredentials,omitempty"`

	// Resources configures CPU and memory for the ingress DaemonSet.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// CertificateStatus tracks the TLS certificate state for a domain.
type CertificateStatus struct {
	// Domain is the domain name.
	Domain string `json:"domain"`

	// Ready indicates whether the certificate is issued and valid.
	Ready bool `json:"ready"`

	// Message provides additional details about the certificate state.
	// +optional
	Message string `json:"message,omitempty"`
}

// HanzoIngressSpec defines the desired state of HanzoIngress.
type HanzoIngressSpec struct {
	// Domains defines the domain configurations for multi-tenant routing.
	// +kubebuilder:validation:MinItems=1
	Domains []DomainConfig `json:"domains"`

	// IngressClassName specifies the Ingress class to use.
	// +kubebuilder:default="nginx"
	// +optional
	IngressClassName string `json:"ingressClassName,omitempty"`

	// ClusterIssuer is the cert-manager ClusterIssuer for TLS certificates.
	// +kubebuilder:default="letsencrypt-prod"
	// +optional
	ClusterIssuer string `json:"clusterIssuer,omitempty"`

	// IngressDaemonSet configures the optional DaemonSet-based ingress controller.
	// +optional
	IngressDaemonSet *IngressDaemonSetSpec `json:"ingressDaemonSet,omitempty"`

	// Labels are additional labels applied to all managed resources.
	// +optional
	Labels map[string]string `json:"labels,omitempty"`

	// Annotations are additional annotations applied to all managed Ingress resources.
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`
}

// HanzoIngressStatus defines the observed state of HanzoIngress.
type HanzoIngressStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ManagedIngresses is the total number of Ingress resources managed.
	// +optional
	ManagedIngresses int32 `json:"managedIngresses,omitempty"`

	// CertificateStatuses tracks certificate state per domain.
	// +optional
	CertificateStatuses []CertificateStatus `json:"certificateStatuses,omitempty"`

	// Conditions represent the latest observations of the ingress state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hing
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ingresses",type=integer,JSONPath=`.status.managedIngresses`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoIngress is the Schema for the hanzoingresses API.
// It manages multi-tenant domain routing with TLS.
type HanzoIngress struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoIngressSpec   `json:"spec,omitempty"`
	Status HanzoIngressStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoIngressList contains a list of HanzoIngress.
type HanzoIngressList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoIngress `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoIngress{}, &HanzoIngressList{})
}
