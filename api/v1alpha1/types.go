package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// ReloadStrategy defines how the gateway deployment is reloaded.
type ReloadStrategy string

const (
	// ReloadStrategyRollout triggers a rolling restart via annotation change.
	ReloadStrategyRollout ReloadStrategy = "Rollout"

	// ReloadStrategyHotReload signals the gateway to pick up config changes without restart.
	ReloadStrategyHotReload ReloadStrategy = "HotReload"
)

// IngressProvider identifies the ingress implementation in use.
type IngressProvider string

const (
	// IngressProviderTraefik means Traefik handles routing natively via K8s Ingress resources.
	// The operator watches Ingress resources for status reporting only.
	IngressProviderTraefik IngressProvider = "traefik"

	// IngressProviderCustom means the operator generates ingress.json for the custom Go proxy.
	IngressProviderCustom IngressProvider = "custom"
)

// IngressSelector defines how to filter Ingress resources.
type IngressSelector struct {
	// MatchLabels selects Ingress resources with these labels.
	// +optional
	MatchLabels map[string]string `json:"matchLabels,omitempty"`
}

// NamespaceFilter defines which namespaces to watch.
type NamespaceFilter struct {
	// Include lists namespaces to watch. Empty means all namespaces.
	// +optional
	Include []string `json:"include,omitempty"`
}

// OutputSpec defines where to write the generated config.
type OutputSpec struct {
	// ConfigMapName is the name of the ConfigMap to create/update.
	ConfigMapName string `json:"configMapName"`

	// Key is the data key within the ConfigMap. Defaults to "ingress.json".
	// +optional
	Key string `json:"key,omitempty"`
}

// TargetSpec identifies the gateway Deployment to reload.
type TargetSpec struct {
	// DeploymentName is the name of the gateway Deployment.
	DeploymentName string `json:"deploymentName"`

	// ReloadStrategy controls how config changes are applied.
	// +optional
	ReloadStrategy ReloadStrategy `json:"reloadStrategy,omitempty"`
}

// DefaultsSpec provides default values for route generation.
type DefaultsSpec struct {
	// BackendScheme is the default scheme for backend URLs. Defaults to "http".
	// +optional
	BackendScheme string `json:"backendScheme,omitempty"`

	// AddHeaders are headers added to every proxied request.
	// +optional
	AddHeaders map[string]string `json:"addHeaders,omitempty"`

	// Timeout is the default backend timeout. Defaults to "30s".
	// +optional
	Timeout string `json:"timeout,omitempty"`
}

// GatewayConfigSpec defines the desired state of GatewayConfig.
type GatewayConfigSpec struct {
	// IngressProvider is the ingress implementation to use.
	// "traefik" means Traefik handles routing natively via K8s Ingress resources.
	// "custom" means the operator generates ingress.json for the custom proxy.
	// +kubebuilder:default=traefik
	// +optional
	IngressProvider IngressProvider `json:"ingressProvider,omitempty"`

	// IngressSelector filters which Ingress resources to process.
	// +optional
	IngressSelector *IngressSelector `json:"ingressSelector,omitempty"`

	// Namespaces restricts which namespaces are watched.
	// +optional
	Namespaces *NamespaceFilter `json:"namespaces,omitempty"`

	// Output defines where the generated config is stored.
	Output OutputSpec `json:"output"`

	// Target identifies the gateway Deployment to reload.
	Target TargetSpec `json:"target"`

	// Defaults provides fallback values for route generation.
	// +optional
	Defaults *DefaultsSpec `json:"defaults,omitempty"`
}

// RouteConflict records a detected routing conflict.
type RouteConflict struct {
	// Host is the conflicting hostname.
	Host string `json:"host"`

	// Path is the conflicting path.
	Path string `json:"path"`

	// Backends lists the competing backend URLs.
	Backends []string `json:"backends"`

	// Winner is the backend that was selected.
	Winner string `json:"winner"`
}

// GatewayConfigStatus defines the observed state of GatewayConfig.
type GatewayConfigStatus struct {
	// IngressProvider is the active ingress provider observed during the last reconciliation.
	// +optional
	IngressProvider IngressProvider `json:"ingressProvider,omitempty"`

	// RouteCount is the number of routes in the current config.
	// +optional
	RouteCount int `json:"routeCount,omitempty"`

	// LastObservedHash is the SHA256 hash of the rendered config from the last reconciliation.
	// For "custom" mode this is the hash of the applied ConfigMap data.
	// For "traefik" mode this is the hash of the observed Ingress state.
	// +optional
	LastObservedHash string `json:"lastObservedHash,omitempty"`

	// LastAppliedHash is the SHA256 hash of the last applied config.
	// Only set when ingressProvider is "custom".
	// +optional
	LastAppliedHash string `json:"lastAppliedHash,omitempty"`

	// Conflicts lists detected routing conflicts.
	// +optional
	Conflicts []RouteConflict `json:"conflicts,omitempty"`

	// SkippedIngresses lists Ingress resources that were skipped (e.g., disabled).
	// +optional
	SkippedIngresses []string `json:"skippedIngresses,omitempty"`

	// LastReconcileTime is when the last reconciliation completed.
	// +optional
	LastReconcileTime *metav1.Time `json:"lastReconcileTime,omitempty"`

	// Conditions represent the latest available observations.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:scope=Namespaced,shortName=gwc
// +kubebuilder:printcolumn:name="Provider",type=string,JSONPath=`.status.ingressProvider`
// +kubebuilder:printcolumn:name="Routes",type=integer,JSONPath=`.status.routeCount`
// +kubebuilder:printcolumn:name="Hash",type=string,JSONPath=`.status.lastObservedHash`,priority=1
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// GatewayConfig is the Schema for the gatewayconfigs API.
type GatewayConfig struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   GatewayConfigSpec   `json:"spec,omitempty"`
	Status GatewayConfigStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// GatewayConfigList contains a list of GatewayConfig.
type GatewayConfigList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []GatewayConfig `json:"items"`
}
