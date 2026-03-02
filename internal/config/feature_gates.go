package config

// FeatureGates controls which optional features are enabled in the operator.
type FeatureGates struct {
	KMSIntegration    bool
	ServiceMonitors   bool
	NetworkPolicies   bool
	Webhooks          bool
	ZAPSidecar        bool
	BlockchainNetwork bool
	MPCCluster        bool
}

// DefaultFeatureGates provides the default feature gate configuration.
var DefaultFeatureGates = FeatureGates{
	KMSIntegration:    true,
	ServiceMonitors:   false,
	NetworkPolicies:   true,
	Webhooks:          false,
	ZAPSidecar:        true,
	BlockchainNetwork: true,
	MPCCluster:        true,
}
