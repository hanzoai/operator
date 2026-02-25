package controllers

import (
	"crypto/sha256"
	"encoding/json"
	"fmt"
	"sort"
	"strings"

	networkingv1 "k8s.io/api/networking/v1"

	gatewayv1alpha1 "github.com/hanzoai/hanzo-operator/api/v1alpha1"
)

// GatewayConfig is the top-level generated config for the Hanzo Gateway.
type GatewayIngressConfig struct {
	Listen     string       `json:"listen"`
	HealthPath string       `json:"health_path"`
	Routes     []HostRoutes `json:"routes"`
}

// HostRoutes groups backends under a single host.
type HostRoutes struct {
	Host     string    `json:"host"`
	Backends []Backend `json:"backends"`
}

// Backend is a single route entry.
type Backend struct {
	Path    string   `json:"path"`
	URL     string   `json:"url"`
	Methods []string `json:"methods,omitempty"`
	Timeout string   `json:"timeout,omitempty"`
}

// routeKey uniquely identifies a route for conflict detection.
type routeKey struct {
	Host string
	Path string
}

// renderResult holds the output of a render pass.
type renderResult struct {
	Config    GatewayIngressConfig
	Hash      string
	JSON      []byte
	Conflicts []gatewayv1alpha1.RouteConflict
	Skipped   []string
}

// renderConfig builds the Hanzo Gateway ingress.json from a set of Ingress resources.
func renderConfig(
	ingresses []networkingv1.Ingress,
	defaults *gatewayv1alpha1.DefaultsSpec,
	selector *gatewayv1alpha1.IngressSelector,
) (*renderResult, error) {
	scheme := "http"
	defaultTimeout := ""
	if defaults != nil {
		if defaults.BackendScheme != "" {
			scheme = defaults.BackendScheme
		}
		if defaults.Timeout != "" {
			defaultTimeout = defaults.Timeout
		}
	}

	// Collect all route entries, keyed by (host, path) for conflict detection.
	type routeEntry struct {
		Backend Backend
		Source  string // namespace/name of the Ingress
	}
	routeMap := make(map[routeKey][]routeEntry)
	var skipped []string

	for i := range ingresses {
		ing := &ingresses[i]
		ref := fmt.Sprintf("%s/%s", ing.Namespace, ing.Name)

		// Check gateway.hanzo.ai/enabled annotation.
		if v, ok := ing.Annotations["gateway.hanzo.ai/enabled"]; ok && v == "false" {
			skipped = append(skipped, ref)
			continue
		}

		// Check label selector match if configured.
		if selector != nil && len(selector.MatchLabels) > 0 {
			if !labelsMatch(ing.Labels, selector.MatchLabels) {
				// If no explicit enabled annotation, skip non-matching.
				if _, ok := ing.Annotations["gateway.hanzo.ai/enabled"]; !ok {
					skipped = append(skipped, ref)
					continue
				}
			}
		}

		// Parse per-Ingress annotations.
		var methods []string
		if m, ok := ing.Annotations["gateway.hanzo.ai/methods"]; ok && m != "" {
			for _, method := range strings.Split(m, ",") {
				methods = append(methods, strings.TrimSpace(method))
			}
		}
		timeout := defaultTimeout
		if t, ok := ing.Annotations["gateway.hanzo.ai/timeout"]; ok && t != "" {
			timeout = t
		}

		for _, rule := range ing.Spec.Rules {
			host := rule.Host
			if host == "" {
				host = "*"
			}
			if rule.HTTP == nil {
				continue
			}
			for _, p := range rule.HTTP.Paths {
				path := p.Path
				if path == "" {
					path = "/"
				}

				// Build backend URL.
				svcName := p.Backend.Service.Name
				svcPort := int32(80)
				if p.Backend.Service.Port.Number != 0 {
					svcPort = p.Backend.Service.Port.Number
				}
				url := fmt.Sprintf("%s://%s.%s.svc.cluster.local:%d",
					scheme, svcName, ing.Namespace, svcPort)

				key := routeKey{Host: host, Path: path}
				entry := routeEntry{
					Backend: Backend{
						Path:    path,
						URL:     url,
						Methods: methods,
						Timeout: timeout,
					},
					Source: ref,
				}
				routeMap[key] = append(routeMap[key], entry)
			}
		}
	}

	// Detect conflicts and select winners (first by source name, deterministic).
	var conflicts []gatewayv1alpha1.RouteConflict
	winners := make(map[routeKey]Backend)

	for key, entries := range routeMap {
		if len(entries) > 1 {
			// Sort entries by source name for deterministic winner.
			sort.Slice(entries, func(i, j int) bool {
				return entries[i].Source < entries[j].Source
			})
			var backends []string
			for _, e := range entries {
				backends = append(backends, fmt.Sprintf("%s (%s)", e.Backend.URL, e.Source))
			}
			conflicts = append(conflicts, gatewayv1alpha1.RouteConflict{
				Host:     key.Host,
				Path:     key.Path,
				Backends: backends,
				Winner:   entries[0].Backend.URL,
			})
		}
		// Winner is first entry (alphabetically by source).
		sort.Slice(entries, func(i, j int) bool {
			return entries[i].Source < entries[j].Source
		})
		winners[key] = entries[0].Backend
	}

	// Group by host.
	hostMap := make(map[string][]Backend)
	for key, backend := range winners {
		hostMap[key.Host] = append(hostMap[key.Host], backend)
	}

	// Build sorted output: hosts alphabetically, paths by length descending (longest first).
	var hosts []string
	for h := range hostMap {
		hosts = append(hosts, h)
	}
	sort.Strings(hosts)

	var routes []HostRoutes
	for _, host := range hosts {
		backends := hostMap[host]
		sort.Slice(backends, func(i, j int) bool {
			if len(backends[i].Path) != len(backends[j].Path) {
				return len(backends[i].Path) > len(backends[j].Path) // longer paths first
			}
			return backends[i].Path < backends[j].Path // alphabetical tiebreak
		})
		routes = append(routes, HostRoutes{
			Host:     host,
			Backends: backends,
		})
	}

	// Sort conflicts deterministically.
	sort.Slice(conflicts, func(i, j int) bool {
		if conflicts[i].Host != conflicts[j].Host {
			return conflicts[i].Host < conflicts[j].Host
		}
		return conflicts[i].Path < conflicts[j].Path
	})

	sort.Strings(skipped)

	config := GatewayIngressConfig{
		Listen:     ":8080",
		HealthPath: "/__health",
		Routes:     routes,
	}

	data, err := json.MarshalIndent(config, "", "  ")
	if err != nil {
		return nil, fmt.Errorf("marshaling config: %w", err)
	}

	hash := fmt.Sprintf("%x", sha256.Sum256(data))

	return &renderResult{
		Config:    config,
		Hash:      hash,
		JSON:      data,
		Conflicts: conflicts,
		Skipped:   skipped,
	}, nil
}

// labelsMatch returns true if all required labels are present on the resource.
func labelsMatch(resourceLabels, required map[string]string) bool {
	for k, v := range required {
		if resourceLabels[k] != v {
			return false
		}
	}
	return true
}
