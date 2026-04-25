package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// ValidatorSpec defines the validator node configuration.
type ValidatorSpec struct {
	// Image defines the validator container image.
	// +kubebuilder:validation:Required
	Image ImageSpec `json:"image"`

	// Replicas is the number of validator nodes.
	// +kubebuilder:default=3
	// +kubebuilder:validation:Minimum=1
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Resources configures CPU and memory for validators.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// Storage configures persistent storage for validator data.
	// +optional
	Storage *StorageSpec `json:"storage,omitempty"`

	// BootstrapNodes lists initial peer addresses for network bootstrapping.
	// +optional
	BootstrapNodes []string `json:"bootstrapNodes,omitempty"`

	// StakingPort is the port used for staking connections.
	// +kubebuilder:default=9631
	// +optional
	StakingPort int32 `json:"stakingPort,omitempty"`

	// HTTPPort is the port used for HTTP API.
	// +kubebuilder:default=9630
	// +optional
	HTTPPort int32 `json:"httpPort,omitempty"`
}

// ChainSpec defines a blockchain chain configuration.
type ChainSpec struct {
	// Name is the chain identifier.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// VMID is the virtual machine identifier for the chain.
	// +kubebuilder:validation:Required
	VMID string `json:"vmID"`

	// Genesis is the raw genesis configuration JSON.
	// +kubebuilder:validation:Required
	Genesis string `json:"genesis"`

	// SubnetID is the subnet this chain belongs to.
	// +optional
	SubnetID string `json:"subnetID,omitempty"`
}

// IndexerSpec configures the blockchain indexer.
type IndexerSpec struct {
	// Enabled controls whether the indexer is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the indexer container image.
	// +kubebuilder:default="ghcr.io/hanzoai/indexer:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// Resources configures CPU and memory for the indexer.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// ExplorerSpec configures the blockchain explorer.
type ExplorerSpec struct {
	// Enabled controls whether the explorer is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// BackendImage is the explorer backend container image.
	// +kubebuilder:default="ghcr.io/hanzoai/explorer-api:latest"
	// +optional
	BackendImage string `json:"backendImage,omitempty"`

	// FrontendImage is the explorer frontend container image.
	// +kubebuilder:default="ghcr.io/hanzoai/explorer-ui:latest"
	// +optional
	FrontendImage string `json:"frontendImage,omitempty"`

	// Resources configures CPU and memory for the explorer.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`

	// PostgresStorage configures storage for the explorer database.
	// +optional
	PostgresStorage *StorageSpec `json:"postgresStorage,omitempty"`
}

// BridgeSpec configures the cross-chain bridge.
type BridgeSpec struct {
	// Enabled controls whether the bridge is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the bridge container image.
	// +kubebuilder:default="ghcr.io/hanzoai/bridge:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// Resources configures CPU and memory for the bridge.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// BootnodeSpec configures the network bootnode.
type BootnodeSpec struct {
	// Enabled controls whether the bootnode is deployed.
	// +optional
	Enabled bool `json:"enabled,omitempty"`

	// Image is the bootnode container image.
	// +kubebuilder:default="ghcr.io/hanzoai/bootnode:latest"
	// +optional
	Image string `json:"image,omitempty"`

	// Resources configures CPU and memory for the bootnode.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// HanzoNetworkSpec defines the desired state of HanzoNetwork.
type HanzoNetworkSpec struct {
	// NetworkID is the unique network identifier.
	// +kubebuilder:validation:Required
	NetworkID string `json:"networkID"`

	// Validators configures the validator node set.
	// +kubebuilder:validation:Required
	Validators ValidatorSpec `json:"validators"`

	// Chains defines the chains in the network.
	// +optional
	Chains []ChainSpec `json:"chains,omitempty"`

	// Indexer configures the optional blockchain indexer.
	// +optional
	Indexer *IndexerSpec `json:"indexer,omitempty"`

	// Explorer configures the optional blockchain explorer.
	// +optional
	Explorer *ExplorerSpec `json:"explorer,omitempty"`

	// Bridge configures the optional cross-chain bridge.
	// +optional
	Bridge *BridgeSpec `json:"bridge,omitempty"`

	// Bootnode configures the optional network bootnode.
	// +optional
	Bootnode *BootnodeSpec `json:"bootnode,omitempty"`

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

// HanzoNetworkStatus defines the observed state of HanzoNetwork.
type HanzoNetworkStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ActiveValidators is the number of running validator nodes.
	// +optional
	ActiveValidators int32 `json:"activeValidators,omitempty"`

	// BootstrapComplete indicates whether the network bootstrap has finished.
	// +optional
	BootstrapComplete bool `json:"bootstrapComplete,omitempty"`

	// ChainCount is the number of configured chains.
	// +optional
	ChainCount int32 `json:"chainCount,omitempty"`

	// Conditions represent the latest observations of the network's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=hnet
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Validators",type=integer,JSONPath=`.status.activeValidators`
// +kubebuilder:printcolumn:name="Chains",type=integer,JSONPath=`.status.chainCount`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// HanzoNetwork is the Schema for the hanzonetworks API.
// It manages a blockchain network with validators, chains, and supporting services.
type HanzoNetwork struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   HanzoNetworkSpec   `json:"spec,omitempty"`
	Status HanzoNetworkStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// HanzoNetworkList contains a list of HanzoNetwork.
type HanzoNetworkList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []HanzoNetwork `json:"items"`
}

func init() {
	SchemeBuilder.Register(&HanzoNetwork{}, &HanzoNetworkList{})
}
