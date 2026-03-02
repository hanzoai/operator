package manifests

const (
	// Standard Kubernetes recommended labels.
	labelName      = "app.kubernetes.io/name"
	labelInstance  = "app.kubernetes.io/instance"
	labelComponent = "app.kubernetes.io/component"
	labelPartOf    = "app.kubernetes.io/part-of"
	labelVersion   = "app.kubernetes.io/version"
	labelManagedBy = "app.kubernetes.io/managed-by"

	managedByValue = "hanzo-operator"
)

// StandardLabels returns the full set of app.kubernetes.io/* labels for a
// managed resource. Empty values are omitted.
func StandardLabels(name, component, partOf, version string) map[string]string {
	labels := map[string]string{
		labelName:      name,
		labelInstance:  name,
		labelManagedBy: managedByValue,
	}
	if component != "" {
		labels[labelComponent] = component
	}
	if partOf != "" {
		labels[labelPartOf] = partOf
	}
	if version != "" {
		labels[labelVersion] = version
	}
	return labels
}

// SelectorLabels returns the minimal label set used in pod selectors.
// These labels must be immutable after creation.
func SelectorLabels(name string) map[string]string {
	return map[string]string{
		labelName:     name,
		labelInstance: name,
	}
}

// MergeLabels merges multiple label maps into a single map. Later maps
// take precedence when keys collide.
func MergeLabels(maps ...map[string]string) map[string]string {
	out := make(map[string]string)
	for _, m := range maps {
		for k, v := range m {
			out[k] = v
		}
	}
	return out
}
