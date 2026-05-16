// Copyright 2026 Hanzo AI.
// Licensed under the Apache License, Version 2.0.
//
// Hand-written deepcopy methods for Brain and Bot. controller-gen will
// regenerate zz_generated.deepcopy.go from scratch and these methods
// will move there on the next `make generate` run.

package v1alpha1

import (
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	runtime "k8s.io/apimachinery/pkg/runtime"
)

// --- Brain -------------------------------------------------------------

// DeepCopyInto copies the receiver into out. in must be non-nil.
func (in *Brain) DeepCopyInto(out *Brain) {
	*out = *in
	out.TypeMeta = in.TypeMeta
	in.ObjectMeta.DeepCopyInto(&out.ObjectMeta)
	in.Spec.DeepCopyInto(&out.Spec)
	in.Status.DeepCopyInto(&out.Status)
}

// DeepCopy returns a deep copy of the receiver.
func (in *Brain) DeepCopy() *Brain {
	if in == nil {
		return nil
	}
	out := new(Brain)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyObject implements runtime.Object.
func (in *Brain) DeepCopyObject() runtime.Object {
	if c := in.DeepCopy(); c != nil {
		return c
	}
	return nil
}

// DeepCopyInto copies the receiver into out.
func (in *BrainList) DeepCopyInto(out *BrainList) {
	*out = *in
	out.TypeMeta = in.TypeMeta
	in.ListMeta.DeepCopyInto(&out.ListMeta)
	if in.Items != nil {
		out.Items = make([]Brain, len(in.Items))
		for i := range in.Items {
			in.Items[i].DeepCopyInto(&out.Items[i])
		}
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BrainList) DeepCopy() *BrainList {
	if in == nil {
		return nil
	}
	out := new(BrainList)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyObject implements runtime.Object.
func (in *BrainList) DeepCopyObject() runtime.Object {
	if c := in.DeepCopy(); c != nil {
		return c
	}
	return nil
}

// DeepCopyInto copies the receiver into out.
func (in *BrainSpec) DeepCopyInto(out *BrainSpec) {
	*out = *in
	in.Image.DeepCopyInto(&out.Image)
	if in.Replicas != nil {
		v := *in.Replicas
		out.Replicas = &v
	}
	out.Storage = in.Storage
	if in.Resources != nil {
		out.Resources = new(ResourceRequirements)
		in.Resources.DeepCopyInto(out.Resources)
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BrainSpec) DeepCopy() *BrainSpec {
	if in == nil {
		return nil
	}
	out := new(BrainSpec)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyInto copies the receiver into out.
func (in *BrainStatus) DeepCopyInto(out *BrainStatus) {
	*out = *in
	if in.Conditions != nil {
		out.Conditions = make([]metav1.Condition, len(in.Conditions))
		for i := range in.Conditions {
			in.Conditions[i].DeepCopyInto(&out.Conditions[i])
		}
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BrainStatus) DeepCopy() *BrainStatus {
	if in == nil {
		return nil
	}
	out := new(BrainStatus)
	in.DeepCopyInto(out)
	return out
}

// --- Bot ---------------------------------------------------------------

// DeepCopyInto copies the receiver into out.
func (in *Bot) DeepCopyInto(out *Bot) {
	*out = *in
	out.TypeMeta = in.TypeMeta
	in.ObjectMeta.DeepCopyInto(&out.ObjectMeta)
	in.Spec.DeepCopyInto(&out.Spec)
	in.Status.DeepCopyInto(&out.Status)
}

// DeepCopy returns a deep copy of the receiver.
func (in *Bot) DeepCopy() *Bot {
	if in == nil {
		return nil
	}
	out := new(Bot)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyObject implements runtime.Object.
func (in *Bot) DeepCopyObject() runtime.Object {
	if c := in.DeepCopy(); c != nil {
		return c
	}
	return nil
}

// DeepCopyInto copies the receiver into out.
func (in *BotList) DeepCopyInto(out *BotList) {
	*out = *in
	out.TypeMeta = in.TypeMeta
	in.ListMeta.DeepCopyInto(&out.ListMeta)
	if in.Items != nil {
		out.Items = make([]Bot, len(in.Items))
		for i := range in.Items {
			in.Items[i].DeepCopyInto(&out.Items[i])
		}
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BotList) DeepCopy() *BotList {
	if in == nil {
		return nil
	}
	out := new(BotList)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyObject implements runtime.Object.
func (in *BotList) DeepCopyObject() runtime.Object {
	if c := in.DeepCopy(); c != nil {
		return c
	}
	return nil
}

// DeepCopyInto copies the receiver into out.
func (in *BotSpec) DeepCopyInto(out *BotSpec) {
	*out = *in
	out.Brain = in.Brain
	in.Image.DeepCopyInto(&out.Image)
	if in.Channels != nil {
		out.Channels = make([]BotChannel, len(in.Channels))
		for i := range in.Channels {
			in.Channels[i].DeepCopyInto(&out.Channels[i])
		}
	}
	if in.Replicas != nil {
		v := *in.Replicas
		out.Replicas = &v
	}
	if in.Resources != nil {
		out.Resources = new(ResourceRequirements)
		in.Resources.DeepCopyInto(out.Resources)
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BotSpec) DeepCopy() *BotSpec {
	if in == nil {
		return nil
	}
	out := new(BotSpec)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyInto copies the receiver into out.
func (in *BotStatus) DeepCopyInto(out *BotStatus) {
	*out = *in
	if in.Conditions != nil {
		out.Conditions = make([]metav1.Condition, len(in.Conditions))
		for i := range in.Conditions {
			in.Conditions[i].DeepCopyInto(&out.Conditions[i])
		}
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BotStatus) DeepCopy() *BotStatus {
	if in == nil {
		return nil
	}
	out := new(BotStatus)
	in.DeepCopyInto(out)
	return out
}

// DeepCopyInto copies the receiver into out.
func (in *BotChannel) DeepCopyInto(out *BotChannel) {
	*out = *in
	if in.Config != nil {
		out.Config = make(map[string]string, len(in.Config))
		for k, v := range in.Config {
			out.Config[k] = v
		}
	}
}

// DeepCopy returns a deep copy of the receiver.
func (in *BotChannel) DeepCopy() *BotChannel {
	if in == nil {
		return nil
	}
	out := new(BotChannel)
	in.DeepCopyInto(out)
	return out
}
