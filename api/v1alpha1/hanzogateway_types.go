package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// AuthPolicyType identifies the authentication mechanism for a gateway route.
// +kubebuilder:validation:Enum=jwt;apikey;oidc;none
type AuthPolicyType string

const (
	AuthPolicyTypeJWT    AuthPolicyType = "jwt"
	AuthPolicyTypeAPIKey AuthPolicyType = "apikey"
	AuthPolicyTypeOIDC   AuthPolicyType = "oidc"
	AuthPolicyTypeNone   AuthPolicyType = "none"
)

// GatewayRoute defines a single route in the API gateway.
type GatewayRoute struct {
	// Prefix is the URL path prefix to match.
	// +kubebuilder:validation:Required
	Prefix string `json:"prefix"`

	// Backend is the target Kubernetes Service name (or name:port).
	// +kubebuilder:validation:Required
	Backend string `json:"backend"`

	// Methods restricts the route to the listed HTTP methods.
	// An empty list matches all methods.
	// +optional
	Methods []string `json:"methods,omitempty"`

	// StripPrefix removes the matched prefix before forwarding to the backend.
	// +optional
	StripPrefix bool `json:"stripPrefix,omitempty"`

	// RateLimit applies per-route rate limiting. Overrides global rate limits.
	// +optional
	RateLimit *RateLimit `json:"rateLimit,omitempty"`

	// AuthPolicy names the AuthPolicy to apply to this route.
	// Must reference an AuthPolicy defined in the gateway spec.
	// +optional
	AuthPolicy string `json:"authPolicy,omitempty"`
}

// RateLimit defines rate limiting parameters.
type RateLimit struct {
	// Name identifies this rate limit configuration.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// MaxRate is the maximum number of requests allowed in the window.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Minimum=1
	MaxRate int32 `json:"maxRate"`

	// Every defines the sliding window duration (e.g. "1m", "1h").
	// +kubebuilder:validation:Required
	Every string `json:"every"`

	// ClientMaxRate is the per-client rate limit within the window.
	// +kubebuilder:validation:Minimum=1
	// +optional
	ClientMaxRate *int32 `json:"clientMaxRate,omitempty"`
}

// AuthPolicy defines an authentication policy for gateway routes.
type AuthPolicy struct {
	// Name uniquely identifies this auth policy.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Type selects the authentication mechanism.
	// +kubebuilder:validation:Required
	Type AuthPolicyType `json:"type"`

	// IAMEndpoint is the IAM server URL (used with oidc and jwt types).
	// +optional
	IAMEndpoint string `json:"iamEndpoint,omitempty"`

	// JWKSURL is the JSON Web Key Set URL for JWT validation.
	// +optional
	JWKSURL string `json:"jwksURL,omitempty"`
}

// HanzoGatewaySpec defines the desired state of HanzoGateway.
type HanzoGatewaySpec struct {
	// Image defines the container image for the gateway.
	// +optional
	Image *ImageSpec `json:"image,omitempty"`

	// Replicas is the desired number of gateway replicas.
	// +kubebuilder:default=2
	// +kubebuilder:validation:Minimum=1
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Routes defines the routing table for the gateway.
	// +optional
	Routes []GatewayRoute `json:"routes,omitempty"`

	// RateLimits defines global rate limit configurations.
	// +optional
	RateLimits []RateLimit `json:"rateLimits,omitempty"`

	// AuthPolicies defines authentication policies referenced by routes.
	// +optional
	AuthPolicies []AuthPolicy `json:"authPolicies,omitempty"`

	// Ingress configures external access via an Ingress resource.
	// +optional
	Ingress *IngressSpec `json:"ingress,omitempty"`

	// Resources configures CPU and memory requests/limits.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// ServiceMonitor configures Prometheus metrics scraping.
	// +optional
	ServiceMonitor *ServiceMonitorSpec `json:"serviceMonitor,omitempty"`
}

// HanzoGatewayStatus defines the observed state of HanzoGateway.
type HanzoGatewayStatus struct {
	// Phase is the current lifecycle phase of the gateway.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyReplicas is the number of replicas that have passed readiness checks.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// Conditions represent the latest observations of the gateway's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`

	// RouteCount is the number of routes currently configured.
	// +optional
	RouteCount int32 `json:"routeCount,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hgw
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Routes",type=integer,JSONPath=`.status.routeCount`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoGateway is the Schema for the hanzogateways API.
// It models an API gateway with routing, rate limiting, and authentication.
type HanzoGateway struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoGatewaySpec   `json:"spec,omitempty"`
	Status HanzoGatewayStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoGatewayList contains a list of HanzoGateway.
type HanzoGatewayList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoGateway `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoGateway{}, &HanzoGatewayList{})
}
