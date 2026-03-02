package controller

import (
	"time"

	corev1 "k8s.io/api/core/v1"
	networkingv1 "k8s.io/api/networking/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	"github.com/hanzoai/operator/internal/manifests"
)

// commonLabels builds the standard label set used by the pre-existing
// gateway, ingress, MPC, and platform controllers. It delegates to the
// manifests package and merges any extra labels from the CR spec.
func commonLabels(name, component string, extra map[string]string) map[string]string {
	return manifests.MergeLabels(
		manifests.StandardLabels(name, component, "", ""),
		extra,
	)
}

// selectorLabels returns minimal selector labels for a controller name
// and component. Used by the pre-existing ingress, MPC, and platform
// controllers.
func selectorLabels(name, component string) map[string]string {
	return map[string]string{
		"app.kubernetes.io/name":      name,
		"app.kubernetes.io/instance":  name,
		"app.kubernetes.io/component": component,
	}
}

// imageTag returns the tag or "latest" if empty.
func imageTag(tag string) string {
	if tag == "" {
		return "latest"
	}
	return tag
}

// applyImagePullSecrets adds image pull secrets to a PodSpec from a list
// of secret name strings (as used by HanzoMPCSpec.ImagePullSecrets).
func applyImagePullSecrets(spec *corev1.PodSpec, secrets []string) {
	for _, s := range secrets {
		spec.ImagePullSecrets = append(spec.ImagePullSecrets, corev1.LocalObjectReference{Name: s})
	}
}

// setCondition upserts a condition by type into the given conditions slice.
func setCondition(conditions *[]metav1.Condition, cond metav1.Condition) {
	now := metav1.NewTime(time.Now())
	cond.LastTransitionTime = now
	for i, c := range *conditions {
		if c.Type == cond.Type {
			if c.Status != cond.Status || c.Reason != cond.Reason {
				(*conditions)[i] = cond
			}
			return
		}
	}
	*conditions = append(*conditions, cond)
}

// conditionBool returns metav1.ConditionTrue if b is true, else metav1.ConditionFalse.
func conditionBool(b bool) metav1.ConditionStatus {
	if b {
		return metav1.ConditionTrue
	}
	return metav1.ConditionFalse
}

// mergeAnnotations merges multiple annotation maps. Later maps override
// earlier entries.
func mergeAnnotations(maps ...map[string]string) map[string]string {
	return manifests.MergeLabels(maps...)
}

// buildTLSConfig creates an IngressTLS entry for the given hosts.
func buildTLSConfig(hosts []string) []networkingv1.IngressTLS {
	if len(hosts) == 0 {
		return nil
	}
	return []networkingv1.IngressTLS{
		{
			Hosts:      hosts,
			SecretName: hosts[0] + "-tls",
		},
	}
}
