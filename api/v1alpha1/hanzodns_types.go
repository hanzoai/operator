package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// DNSZoneSpec defines a DNS zone managed by the operator.
type DNSZoneSpec struct {
	// Name is the fully qualified domain name (e.g. "example.com").
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// CloudflareZoneID is the Cloudflare zone ID for edge sync.
	// +optional
	CloudflareZoneID string `json:"cloudflareZoneId,omitempty"`
}

// DNSRecordSpec defines a DNS record within a zone.
type DNSRecordSpec struct {
	// Name is the record name (e.g. "www", "@").
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Type is the DNS record type (A, AAAA, CNAME, MX, TXT, SRV, NS, CAA).
	// +kubebuilder:validation:Required
	// +kubebuilder:validation:Enum=A;AAAA;CNAME;MX;TXT;SRV;NS;CAA
	Type string `json:"type"`

	// Content is the record value.
	// +kubebuilder:validation:Required
	Content string `json:"content"`

	// TTL is the time-to-live in seconds.
	// +kubebuilder:default=300
	// +kubebuilder:validation:Minimum=1
	// +optional
	TTL int32 `json:"ttl,omitempty"`

	// Priority is used for MX and SRV records.
	// +optional
	Priority *int32 `json:"priority,omitempty"`

	// Proxied enables Cloudflare proxy for this record.
	// +kubebuilder:default=false
	// +optional
	Proxied bool `json:"proxied,omitempty"`

	// SyncToCloudflare enables syncing this record to Cloudflare.
	// +kubebuilder:default=false
	// +optional
	SyncToCloudflare bool `json:"syncToCloudflare,omitempty"`
}

// CoreDNSSpec configures the CoreDNS deployment.
type CoreDNSSpec struct {
	// Image is the CoreDNS container image.
	// +kubebuilder:default="ghcr.io/hanzoai/dns:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// Replicas is the number of CoreDNS replicas.
	// +kubebuilder:default=2
	// +kubebuilder:validation:Minimum=1
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// APIPort is the REST API port for the hanzodns plugin.
	// +kubebuilder:default=8443
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	APIPort int32 `json:"apiPort,omitempty"`

	// DNSPort is the DNS listener port.
	// +kubebuilder:default=53
	// +kubebuilder:validation:Minimum=1
	// +kubebuilder:validation:Maximum=65535
	// +optional
	DNSPort int32 `json:"dnsPort,omitempty"`

	// Resources configures CPU and memory for CoreDNS pods.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// CloudflareSyncSpec configures Cloudflare edge synchronization.
type CloudflareSyncSpec struct {
	// Enabled controls whether Cloudflare sync is active.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// CredentialsRef references a Secret containing the Cloudflare API token.
	// The secret must have a key named "api-token".
	// +optional
	CredentialsRef *corev1.SecretReference `json:"credentialsRef,omitempty"`
}

// DatabaseSpec configures the PostgreSQL connection for the sync watcher.
type DatabaseSpec struct {
	// Enabled controls whether the PG sync watcher is active.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// CredentialsRef references a Secret containing "database-url" key
	// with the PostgreSQL connection string.
	// +optional
	CredentialsRef *corev1.SecretReference `json:"credentialsRef,omitempty"`

	// SyncInterval is the polling interval for database changes.
	// +kubebuilder:default="30s"
	// +optional
	SyncInterval string `json:"syncInterval,omitempty"`
}

// OIDCSpec configures OIDC JWT validation for the CoreDNS REST API.
type OIDCSpec struct {
	// Enabled controls whether OIDC authentication is enforced.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Issuer is the OIDC issuer URL (e.g. "https://hanzo.id").
	// +optional
	Issuer string `json:"issuer,omitempty"`

	// Audience is the expected JWT audience claim.
	// +optional
	Audience string `json:"audience,omitempty"`
}

// HanzoDNSSpec defines the desired state of HanzoDNS.
type HanzoDNSSpec struct {
	// Zones defines the DNS zones to manage.
	// +optional
	Zones []DNSZoneSpec `json:"zones,omitempty"`

	// CoreDNS configures the CoreDNS deployment.
	// +optional
	CoreDNS *CoreDNSSpec `json:"coredns,omitempty"`

	// Cloudflare configures Cloudflare edge synchronization.
	// +optional
	Cloudflare *CloudflareSyncSpec `json:"cloudflare,omitempty"`

	// Database configures the PostgreSQL sync watcher.
	// +optional
	Database *DatabaseSpec `json:"database,omitempty"`

	// OIDC configures JWT authentication for the REST API.
	// +optional
	OIDC *OIDCSpec `json:"oidc,omitempty"`

	// Ingress configures external access to the DNS API.
	// +optional
	Ingress *IngressSpec `json:"ingress,omitempty"`

	// Labels are additional labels applied to all managed resources.
	// +optional
	Labels map[string]string `json:"labels,omitempty"`

	// Annotations are additional annotations applied to managed resources.
	// +optional
	Annotations map[string]string `json:"annotations,omitempty"`
}

// ZoneSyncStatus tracks the sync state of a DNS zone.
type ZoneSyncStatus struct {
	// Name is the zone domain name.
	Name string `json:"name"`

	// CoreDNSSynced indicates whether the zone is synced to CoreDNS.
	CoreDNSSynced bool `json:"corednsSynced"`

	// CloudflareSynced indicates whether the zone is synced to Cloudflare.
	CloudflareSynced bool `json:"cloudflareSynced"`

	// RecordCount is the number of records in this zone.
	// +optional
	RecordCount int32 `json:"recordCount,omitempty"`

	// LastSyncTime is the last successful sync timestamp.
	// +optional
	LastSyncTime *metav1.Time `json:"lastSyncTime,omitempty"`
}

// HanzoDNSStatus defines the observed state of HanzoDNS.
type HanzoDNSStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ManagedZones is the total number of zones managed.
	// +optional
	ManagedZones int32 `json:"managedZones,omitempty"`

	// CoreDNSReady indicates whether the CoreDNS deployment is available.
	// +optional
	CoreDNSReady bool `json:"corednsReady,omitempty"`

	// ZoneStatuses tracks per-zone sync state.
	// +optional
	ZoneStatuses []ZoneSyncStatus `json:"zoneStatuses,omitempty"`

	// Conditions represent the latest observations of the DNS state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hdns
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Zones",type=integer,JSONPath=`.status.managedZones`
// +kubebuilder:printcolumn:name="CoreDNS",type=boolean,JSONPath=`.status.corednsReady`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoDNS is the Schema for the hanzodns API.
// It manages multi-tenant DNS zones with CoreDNS and Cloudflare edge sync.
type HanzoDNS struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoDNSSpec   `json:"spec,omitempty"`
	Status HanzoDNSStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoDNSList contains a list of HanzoDNS.
type HanzoDNSList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoDNS `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoDNS{}, &HanzoDNSList{})
}
