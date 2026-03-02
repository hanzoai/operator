package v1alpha1

import (
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// DatastoreType identifies the kind of data service.
// +kubebuilder:validation:Enum=postgresql;valkey;docdb;minio;nats;clickhouse
type DatastoreType string

const (
	DatastoreTypePostgreSQL DatastoreType = "postgresql"
	DatastoreTypeValkey     DatastoreType = "valkey"
	DatastoreTypeDocDB      DatastoreType = "docdb"
	DatastoreTypeMinio      DatastoreType = "minio"
	DatastoreTypeNATS       DatastoreType = "nats"
	DatastoreTypeClickhouse DatastoreType = "clickhouse"
)

// BackupSpec configures automated backups for a datastore.
type BackupSpec struct {
	// Enabled controls whether automated backups are active.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Schedule is a cron expression for backup timing.
	// +optional
	Schedule string `json:"schedule,omitempty"`

	// S3Endpoint is the S3-compatible endpoint for backup storage.
	// +optional
	S3Endpoint string `json:"s3Endpoint,omitempty"`

	// S3Bucket is the bucket name for backup storage.
	// +optional
	S3Bucket string `json:"s3Bucket,omitempty"`

	// S3CredentialsSecret references a Secret containing S3 access credentials.
	// +optional
	S3CredentialsSecret string `json:"s3CredentialsSecret,omitempty"`

	// RetentionDays is the number of days to retain backups.
	// +kubebuilder:default=30
	// +kubebuilder:validation:Minimum=1
	// +optional
	RetentionDays *int32 `json:"retentionDays,omitempty"`
}

// HanzoDatastoreSpec defines the desired state of HanzoDatastore.
type HanzoDatastoreSpec struct {
	// Type selects the datastore engine.
	// +kubebuilder:validation:Required
	Type DatastoreType `json:"type"`

	// Image overrides the default container image for the datastore type.
	// When nil, a sensible default image is used per type.
	// +optional
	Image *ImageSpec `json:"image,omitempty"`

	// Replicas is the desired number of datastore replicas.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=1
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Storage configures persistent volume claims.
	// +kubebuilder:validation:Required
	Storage StorageSpec `json:"storage"`

	// Resources configures CPU and memory requests/limits.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// ZAP configures the ZAP sidecar proxy.
	// +optional
	ZAP *ZAPSidecar `json:"zap,omitempty"`

	// Env is a list of environment variables to set in the container.
	// +optional
	Env []corev1.EnvVar `json:"env,omitempty"`

	// EnvFrom is a list of sources to populate environment variables.
	// +optional
	EnvFrom []corev1.EnvFromSource `json:"envFrom,omitempty"`

	// Volumes defines additional volumes to attach to the pod.
	// +optional
	Volumes []corev1.Volume `json:"volumes,omitempty"`

	// VolumeMounts defines additional volume mounts for the main container.
	// +optional
	VolumeMounts []corev1.VolumeMount `json:"volumeMounts,omitempty"`

	// Command overrides the container entrypoint.
	// +optional
	Command []string `json:"command,omitempty"`

	// Args overrides the container arguments.
	// +optional
	Args []string `json:"args,omitempty"`

	// CredentialsSecret is the name of a Secret containing datastore credentials.
	// +optional
	CredentialsSecret string `json:"credentialsSecret,omitempty"`

	// KMSSecrets lists KMS secret references to sync.
	// +optional
	KMSSecrets []KMSSecretRef `json:"kmsSecrets,omitempty"`

	// Backup configures automated backup schedules and storage.
	// +optional
	Backup *BackupSpec `json:"backup,omitempty"`

	// ServiceAliases creates additional Kubernetes Services pointing at this datastore.
	// Useful for backward-compatible DNS names.
	// +optional
	ServiceAliases []string `json:"serviceAliases,omitempty"`

	// Ports overrides the default ports for the datastore.
	// +optional
	Ports []ServicePort `json:"ports,omitempty"`

	// ImagePullSecrets lists references to secrets for pulling container images.
	// +optional
	ImagePullSecrets []corev1.LocalObjectReference `json:"imagePullSecrets,omitempty"`

	// NetworkPolicy configures network-level access control.
	// +optional
	NetworkPolicy *NetworkPolicySpec `json:"networkPolicy,omitempty"`

	// ServiceMonitor configures Prometheus metrics scraping.
	// +optional
	ServiceMonitor *ServiceMonitorSpec `json:"serviceMonitor,omitempty"`

	// PartOf identifies the application this datastore belongs to (app.kubernetes.io/part-of).
	// +optional
	PartOf string `json:"partOf,omitempty"`
}

// HanzoDatastoreStatus defines the observed state of HanzoDatastore.
type HanzoDatastoreStatus struct {
	// Phase is the current lifecycle phase of the datastore.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyReplicas is the number of replicas that have passed readiness checks.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// Conditions represent the latest observations of the datastore's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`

	// ConnectionString is the internal connection string for this datastore.
	// +optional
	ConnectionString string `json:"connectionString,omitempty"`

	// LastBackup records the time of the most recent successful backup.
	// +optional
	LastBackup *metav1.Time `json:"lastBackup,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hds
// +kubebuilder:printcolumn:name="Type",type=string,JSONPath=`.spec.type`
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Storage",type=string,JSONPath=`.spec.storage.size`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoDatastore is the Schema for the hanzodatastores API.
// It models stateful data services such as PostgreSQL, Valkey, DocDB, MinIO, NATS, and ClickHouse.
type HanzoDatastore struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoDatastoreSpec   `json:"spec,omitempty"`
	Status HanzoDatastoreStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoDatastoreList contains a list of HanzoDatastore.
type HanzoDatastoreList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoDatastore `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoDatastore{}, &HanzoDatastoreList{})
}
