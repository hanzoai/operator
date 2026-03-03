package manifests

import (
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	autoscalingv2 "k8s.io/api/autoscaling/v2"
	batchv1 "k8s.io/api/batch/v1"
	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	policyv1 "k8s.io/api/policy/v1"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"
)

// MutateFuncFor returns a controllerutil.MutateFn that copies the spec from
// desired into existing while preserving existing metadata fields that the
// API server manages (UID, resourceVersion, etc.). Labels and annotations
// are merged from desired into existing.
func MutateFuncFor(existing, desired client.Object) controllerutil.MutateFn {
	return func() error {
		// Merge labels and annotations.
		existingLabels := existing.GetLabels()
		if existingLabels == nil {
			existingLabels = make(map[string]string)
		}
		for k, v := range desired.GetLabels() {
			existingLabels[k] = v
		}
		existing.SetLabels(existingLabels)

		existingAnnotations := existing.GetAnnotations()
		if existingAnnotations == nil {
			existingAnnotations = make(map[string]string)
		}
		for k, v := range desired.GetAnnotations() {
			existingAnnotations[k] = v
		}
		existing.SetAnnotations(existingAnnotations)

		// Copy spec fields based on concrete type.
		switch e := existing.(type) {
		case *appsv1.Deployment:
			d, ok := desired.(*appsv1.Deployment)
			if !ok {
				return fmt.Errorf("desired is %T, want *appsv1.Deployment", desired)
			}
			e.Spec.Replicas = d.Spec.Replicas
			e.Spec.MinReadySeconds = d.Spec.MinReadySeconds
			e.Spec.Strategy = d.Spec.Strategy
			e.Spec.Template = d.Spec.Template
			// Selector is immutable after creation; only set if empty.
			if e.Spec.Selector == nil {
				e.Spec.Selector = d.Spec.Selector
			}
			// Ensure template labels are a superset of selector matchLabels.
			if e.Spec.Selector != nil && e.Spec.Selector.MatchLabels != nil {
				if e.Spec.Template.Labels == nil {
					e.Spec.Template.Labels = make(map[string]string)
				}
				for k, v := range e.Spec.Selector.MatchLabels {
					e.Spec.Template.Labels[k] = v
				}
			}

		case *appsv1.StatefulSet:
			d, ok := desired.(*appsv1.StatefulSet)
			if !ok {
				return fmt.Errorf("desired is %T, want *appsv1.StatefulSet", desired)
			}
			e.Spec.Replicas = d.Spec.Replicas
			e.Spec.MinReadySeconds = d.Spec.MinReadySeconds
			e.Spec.UpdateStrategy = d.Spec.UpdateStrategy
			e.Spec.Template = d.Spec.Template
			// Selector and VolumeClaimTemplates are immutable after creation.
			if e.Spec.Selector == nil {
				e.Spec.Selector = d.Spec.Selector
			}
			if len(e.Spec.VolumeClaimTemplates) == 0 {
				e.Spec.VolumeClaimTemplates = d.Spec.VolumeClaimTemplates
			} else if len(d.Spec.VolumeClaimTemplates) > 0 && len(e.Spec.VolumeClaimTemplates) > 0 {
				// VCTs are immutable; remap desired volume mount names to match existing VCT names.
				nameMap := make(map[string]string) // desired name -> existing name
				for i := 0; i < len(d.Spec.VolumeClaimTemplates) && i < len(e.Spec.VolumeClaimTemplates); i++ {
					dName := d.Spec.VolumeClaimTemplates[i].Name
					eName := e.Spec.VolumeClaimTemplates[i].Name
					if dName != eName {
						nameMap[dName] = eName
					}
				}
				for i := range e.Spec.Template.Spec.Containers {
					for j := range e.Spec.Template.Spec.Containers[i].VolumeMounts {
						vm := &e.Spec.Template.Spec.Containers[i].VolumeMounts[j]
						if mapped, ok := nameMap[vm.Name]; ok {
							vm.Name = mapped
						}
					}
				}
			}
			// Ensure template labels are a superset of selector matchLabels.
			if e.Spec.Selector != nil && e.Spec.Selector.MatchLabels != nil {
				if e.Spec.Template.Labels == nil {
					e.Spec.Template.Labels = make(map[string]string)
				}
				for k, v := range e.Spec.Selector.MatchLabels {
					e.Spec.Template.Labels[k] = v
				}
			}

		case *corev1.Service:
			d, ok := desired.(*corev1.Service)
			if !ok {
				return fmt.Errorf("desired is %T, want *corev1.Service", desired)
			}
			e.Spec.Ports = d.Spec.Ports
			e.Spec.Selector = d.Spec.Selector
			e.Spec.Type = d.Spec.Type
			// Preserve ClusterIP — it is immutable once assigned.
			if d.Spec.ClusterIP == "None" {
				e.Spec.ClusterIP = "None"
			}

		case *networkingv1.Ingress:
			d, ok := desired.(*networkingv1.Ingress)
			if !ok {
				return fmt.Errorf("desired is %T, want *networkingv1.Ingress", desired)
			}
			e.Spec = d.Spec

		case *autoscalingv2.HorizontalPodAutoscaler:
			d, ok := desired.(*autoscalingv2.HorizontalPodAutoscaler)
			if !ok {
				return fmt.Errorf("desired is %T, want *autoscalingv2.HorizontalPodAutoscaler", desired)
			}
			e.Spec = d.Spec

		case *policyv1.PodDisruptionBudget:
			d, ok := desired.(*policyv1.PodDisruptionBudget)
			if !ok {
				return fmt.Errorf("desired is %T, want *policyv1.PodDisruptionBudget", desired)
			}
			e.Spec = d.Spec

		case *networkingv1.NetworkPolicy:
			d, ok := desired.(*networkingv1.NetworkPolicy)
			if !ok {
				return fmt.Errorf("desired is %T, want *networkingv1.NetworkPolicy", desired)
			}
			e.Spec = d.Spec

		case *corev1.ConfigMap:
			d, ok := desired.(*corev1.ConfigMap)
			if !ok {
				return fmt.Errorf("desired is %T, want *corev1.ConfigMap", desired)
			}
			e.Data = d.Data
			e.BinaryData = d.BinaryData

		case *batchv1.CronJob:
			d, ok := desired.(*batchv1.CronJob)
			if !ok {
				return fmt.Errorf("desired is %T, want *batchv1.CronJob", desired)
			}
			e.Spec = d.Spec

		default:
			return fmt.Errorf("unsupported type for mutation: %T", existing)
		}

		return nil
	}
}
