// Copyright 2026 Hanzo AI.
// Licensed under the Apache License, Version 2.0.

package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
)

// BotChannel names a single channel the bot listens on (e.g. "slack", "discord").
type BotChannel struct {
	// Name identifies the channel kind.
	// +kubebuilder:validation:Required
	Name string `json:"name"`

	// Config is an opaque blob of channel-specific configuration.
	// The operator passes it to the bot binary via env.
	// +optional
	Config map[string]string `json:"config,omitempty"`
}

// BotBrainRef points at the Brain this bot reads from / writes to.
type BotBrainRef struct {
	// Name is the Brain CR name.
	// +kubebuilder:validation:Required
	Name string `json:"name"`
}

// BotSpec defines the desired state of a Bot. One Deployment per Bot;
// each Bot references exactly one Brain in the same namespace.
type BotSpec struct {
	// Brain references the per-tenant Brain this bot reads/writes.
	// +kubebuilder:validation:Required
	Brain BotBrainRef `json:"brain"`

	// Image defines the container image for the bot binary.
	// +kubebuilder:validation:Required
	Image ImageSpec `json:"image"`

	// Channels lists the channels this bot listens on.
	// +optional
	Channels []BotChannel `json:"channels,omitempty"`

	// Replicas is the desired Deployment size. 0 stops the bot, >=1 starts it.
	// +kubebuilder:default=1
	// +kubebuilder:validation:Minimum=0
	// +optional
	Replicas *int32 `json:"replicas,omitempty"`

	// Resources configures CPU and memory requests/limits.
	// +optional
	Resources *ResourceRequirements `json:"resources,omitempty"`
}

// BotStatus defines the observed state of Bot.
type BotStatus struct {
	// Phase is the current lifecycle phase.
	// +optional
	Phase Phase `json:"phase,omitempty"`

	// ReadyReplicas is the number of replicas that have passed readiness checks.
	// +optional
	ReadyReplicas int32 `json:"readyReplicas,omitempty"`

	// Conditions represent the latest observations of the bot's state.
	// +optional
	Conditions []metav1.Condition `json:"conditions,omitempty"`

	// ObservedGeneration is the most recent generation observed by the controller.
	// +optional
	ObservedGeneration int64 `json:"observedGeneration,omitempty"`
}

// +kubebuilder:object:root=true
// +kubebuilder:subresource:status
// +kubebuilder:resource:shortName=bot
// +kubebuilder:printcolumn:name="Phase",type=string,JSONPath=`.status.phase`
// +kubebuilder:printcolumn:name="Ready",type=integer,JSONPath=`.status.readyReplicas`
// +kubebuilder:printcolumn:name="Brain",type=string,JSONPath=`.spec.brain.name`
// +kubebuilder:printcolumn:name="Age",type=date,JSONPath=`.metadata.creationTimestamp`

// Bot is a single conversational agent backed by a Brain.
type Bot struct {
	metav1.TypeMeta   `json:",inline"`
	metav1.ObjectMeta `json:"metadata,omitempty"`

	Spec   BotSpec   `json:"spec,omitempty"`
	Status BotStatus `json:"status,omitempty"`
}

// +kubebuilder:object:root=true

// BotList contains a list of Bot.
type BotList struct {
	metav1.TypeMeta `json:",inline"`
	metav1.ListMeta `json:"metadata,omitempty"`
	Items           []Bot `json:"items"`
}

func init() {
	SchemeBuilder.Register(&Bot{}, &BotList{})
}
