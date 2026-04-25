package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	"k8s.io/apimachinery/pkg/api/resource"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// Phase represents the lifecycle phase of a managed resource.
// +kubebuilder:validation:Enum=Pending;Creating;Running;Degraded;Deleting
type Phase string

const (
	PhasePending  Phase = "Pending"
	PhaseCreating Phase = "Creating"
	PhaseRunning  Phase = "Running"
	PhaseDegraded Phase = "Degraded"
	PhaseDeleting Phase = "Deleting"
)

// ConditionType constants for hanzo.ai resources.
const (
	ConditionTypeReady       = "Ready"
	ConditionTypeDegraded    = "Degraded"
	ConditionTypeProgressing = "Progressing"
	ConditionTypeReconciled  = "Reconciled"
)

// ImageSpec defines the container image to run.
type ImageSpec struct {
	// Repository is the container image repository.
	// +kubebuilder:validation:Required
	Repository string `json:"repository"`

	// Tag is the container image tag.
	// +kubebuilder:default="latest"
	// +optional
	Tag string `json:"tag,omitempty"`

	// PullPolicy defines when to pull the image.
	// +kubebuilder:default="IfNotPresent"
	// +kubebuilder:validation:Enum=Always;IfNotPresent;Never
	// +optional
	PullPolicy corev1.PullPolicy `json:"pullPolicy,omitempty"`
}

// ResourceRequirements describes CPU and memory resource requests and limits.
type ResourceRequirements struct {
	// Requests describes the minimum amount of compute resources required.
	// +optional
	Requests corev1.ResourceList `json:"requests,omitempty"`

	// Limits describes the maximum amount of compute resources allowed.
	// +optional
	Limits corev1.ResourceList `json:"limits,omitempty"`
}

// ProbeSpec configures a health probe for a container.
type ProbeSpec struct {
	// Path is the HTTP path to probe.
	// +kubebuilder:default="/health"
	// +optional
	Path string `json:"path,omitempty"`

	// Port is the port to probe.
	// +kubebuilder:validation:Required
	Port int32 `json:"port"`

	// InitialDelaySeconds is the number of seconds after the container has
	// started before the probe is initiated.
	// +kubebuilder:default=5
	// +kubebuilder:validation:Minimum=0
	// +optional
	InitialDelaySeconds int32 `json:"initialDelaySeconds,omitempty"`

	// PeriodSeconds is how often (in seconds) to perform the probe.
	// +kubebuilder:default=10
	// +kubebuilder:validation:Minimum=1
	// +optional
	PeriodSeconds int32 `json:"periodSeconds,omitempty"`
}

// KMSSecretRef generates a KMSSecret CR under kms.hanzo.ai/v1alpha1
// to sync secrets from Hanzo KMS into a Kubernetes Secret.
type KMSSecretRef struct {
	// HostAPI is the KMS API endpoint.
	// +kubebuilder:default="https://kms.hanzo.ai/api"
	// +optional
	HostAPI string `json:"hostAPI,omitempty"`

	// ProjectSlug identifies the KMS project.
	// +kubebuilder:validation:Required
	ProjectSlug string `json:"projectSlug"`

	// EnvSlug identifies the KMS environment (e.g. "production", "staging").
	// +kubebuilder:validation:Required
	EnvSlug string `json:"envSlug"`

	// SecretsPath is the path within the KMS project to read secrets from.
	// +kubebuilder:validation:Required
	SecretsPath string `json:"secretsPath"`

	// CredentialsRef references a Kubernetes Secret containing KMS credentials.
	// +kubebuilder:validation:Required
	CredentialsRef corev1.SecretReference `json:"credentialsRef"`

	// ResyncInterval is the interval in seconds between KMS secret re-syncs.
	// +kubebuilder:default=60
	// +kubebuilder:validation:Minimum=10
	// +optional
	ResyncInterval int32 `json:"resyncInterval,omitempty"`

	// ManagedSecretName is the name of the Kubernetes Secret to create/update.
	// +kubebuilder:validation:Required
	ManagedSecretName string `json:"managedSecretName"`
}

// IngressSpec configures ingress for a service.
type IngressSpec struct {
	// Enabled controls whether an Ingress resource is created.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Hosts lists the hostnames for the Ingress.
	// +optional
	Hosts []string `json:"hosts,omitempty"`

	// IngressClassName specifies the Ingress class to use.
	// +kubebuilder:default="ingress"
	// +optional
	IngressClassName string `json:"ingressClassName,omitempty"`

	// TLS enables TLS termination on the Ingress.
	// +kubebuilder:default=true
	// +optional
	TLS bool `json:"tls,omitempty"`

	// ClusterIssuer is the cert-manager ClusterIssuer to use for TLS certificates.
	// +kubebuilder:default="letsencrypt-prod"
	// +optional
	ClusterIssuer string `json:"clusterIssuer,omitempty"`

	// PathRules defines per-path routing rules.
	// +optional
	PathRules []PathRule `json:"pathRules,omitempty"`

	// Annotations are additional annotations applied to the Ingress resource.
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`

	// ZeroTrustPolicy configures zero-trust access control on the Ingress.
	// +optional
	ZeroTrustPolicy *ZeroTrustPolicySpec `json:"zeroTrustPolicy,omitempty"`
}

// PathRule defines a single path-based routing rule within an Ingress.
type PathRule struct {
	// Path is the URL path to match.
	// +kubebuilder:validation:Required
	Path string `json:"path"`

	// PathType specifies how the path is matched.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Enum=Exact;Prefix;ImplementationSpecific
	PathType string `json:"pathType"`

	// Port is the backend service port for this path.
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	Port int32 `json:"port"`

	// ServiceName overrides the default backend service for this path.
	// If empty, the parent HanzoService name is used.
	// +optional
	ServiceName string `json:"serviceName,omitempty"`
}

// ZeroTrustPolicySpec configures identity-aware access control.
type ZeroTrustPolicySpec struct {
	// Enabled controls whether zero-trust policy is enforced.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// AllowedEmails lists email addresses permitted access.
	// +optional
	AllowedEmails []string `json:"allowedEmails,omitempty"`

	// AllowedDomains lists email domains permitted access.
	// +optional
	AllowedDomains []string `json:"allowedDomains,omitempty"`

	// AllowedGroups lists IAM groups permitted access.
	// +optional
	AllowedGroups []string `json:"allowedGroups,omitempty"`

	// SessionDuration controls how long a session remains valid.
	// +kubebuilder:default="24h"
	// +optional
	SessionDuration string `json:"sessionDuration,omitempty"`

	// IAMEndpoint is the identity provider endpoint used for verification.
	// +optional
	IAMEndpoint string `json:"iamEndpoint,omitempty"`
}

// ZAPSidecar configures the ZAP (Zero-latency Access Proxy) sidecar container.
type ZAPSidecar struct {
	// Enabled controls whether the ZAP sidecar is injected.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the ZAP sidecar container image.
	// +kubebuilder:default="ghcr.io/hanzoai/zap:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// Mode selects the ZAP proxy backend type.
	// +kubebuilder:validation:Enum=sql;kv;s3;nats
	// +optional
	Mode string `json:"mode,omitempty"`

	// Port is the port the ZAP sidecar listens on.
	// +kubebuilder:default=9999
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	Port int32 `json:"port,omitempty"`

	// BackendEnvVar is the environment variable name injected into the main
	// container with the ZAP backend connection string.
	// +optional
	BackendEnvVar string `json:"backendEnvVar,omitempty"`

	// Resources configures CPU and memory for the ZAP sidecar.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// Env is a list of additional environment variables for the ZAP sidecar.
	// +optional
	Env []corev1.EnvVar `json:"env,omitempty"`
}

// NetworkPolicySpec configures network-level access control.
type NetworkPolicySpec struct {
	// Enabled controls whether a NetworkPolicy is created.
	// +kubebuilder:default=true
	// +optional
	Enabled *bool `json:"enabled,omitempty"`

	// AllowFrom specifies additional peers allowed to connect.
	// +optional
	AllowFrom []NetworkPolicyPeer `json:"allowFrom,omitempty"`

	// AllowIngress permits all inbound traffic when true.
	// +optional
	AllowIngress bool `json:"allowIngress,omitempty"`

	// AllowIntraNamespace permits traffic from pods in the same namespace.
	// +kubebuilder:default=true
	// +optional
	AllowIntraNamespace *bool `json:"allowIntraNamespace,omitempty"`
}

// NetworkPolicyPeer identifies a peer for network policy rules.
type NetworkPolicyPeer struct {
	// PodSelector selects pods within the peer's namespace.
	// +optional
	PodSelector *metav1.LabelSelector `json:"podSelector,omitempty"`

	// NamespaceSelector selects namespaces for the peer.
	// +optional
	NamespaceSelector *metav1.LabelSelector `json:"namespaceSelector,omitempty"`
}

// AutoscalingSpec configures horizontal pod autoscaling.
type AutoscalingSpec struct {
	// Enabled controls whether an HPA is created.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// MinReplicas is the minimum number of replicas.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=1
	// +optional
	MinReplicas *int32 `json:"minReplicas,omitempty"`

	// MaxReplicas is the maximum number of replicas.
	// +kubebuilder:default=10
	// +kubebuilder:validation:Minimum=1
	// +optional
	MaxReplicas *int32 `json:"maxReplicas,omitempty"`

	// TargetCPUUtilization is the target average CPU utilization percentage.
	// +kubebuilder:default=80
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=100
	// +optional
	TargetCPUUtilization *int32 `json:"targetCPUUtilization,omitempty"`

	// TargetMemoryUtilization is the target average memory utilization percentage.
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=100
	// +optional
	TargetMemoryUtilization *int32 `json:"targetMemoryUtilization,omitempty"`
}

// PodDisruptionBudgetSpec configures a PodDisruptionBudget.
type PodDisruptionBudgetSpec struct {
	// Enabled controls whether a PDB is created.
	// +kubebuilder:default=false
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// MinAvailable is the minimum number of pods that must remain available.
	// +optional
	MinAvailable *int32 `json:"minAvailable,omitempty"`

	// MaxUnavailable is the maximum number of pods that can be unavailable.
	// +optional
	MaxUnavailable *int32 `json:"maxUnavailable,omitempty"`
}

// ServiceMonitorSpec configures Prometheus ServiceMonitor scraping.
type ServiceMonitorSpec struct {
	// Enabled controls whether a ServiceMonitor is created.
	// +kubebuilder:default=false
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// MetricsPort is the named port to scrape metrics from.
	// +kubebuilder:default="metrics"
	// +optional
	MetricsPort string `json:"metricsPort,omitempty"`

	// MetricsPath is the HTTP path to scrape for metrics.
	// +kubebuilder:default="/metrics"
	// +optional
	MetricsPath string `json:"metricsPath,omitempty"`

	// Interval defines how frequently Prometheus scrapes the metrics endpoint.
	// +kubebuilder:default="30s"
	// +optional
	Interval string `json:"interval,omitempty"`
}

// StorageSpec configures persistent storage for a datastore.
type StorageSpec struct {
	// StorageClassName specifies the Kubernetes StorageClass to use.
	// +kubebuilder:default="do-block-storage"
	// +optional
	StorageClassName string `json:"storageClassName,omitempty"`

	// Size is the requested storage capacity.
	// +kubebuilder:validation:Required
	Size resource.Quantity `json:"size"`

	// RetentionPolicy determines whether the PVC is retained or deleted
	// when the datastore is removed.
	// +kubebuilder:default="Retain"
	// +kubebuilder:validation:Enum=Retain;Delete
	// +optional
	RetentionPolicy RetentionPolicy `json:"retentionPolicy,omitempty"`
}

// RetentionPolicy determines PVC lifecycle on resource deletion.
// +kubebuilder:validation:Enum=Retain;Delete
type RetentionPolicy string

const (
	RetentionPolicyRetain RetentionPolicy = "Retain"
	RetentionPolicyDelete RetentionPolicy = "Delete"
)
