package controllers

import (
	"encoding/json"
	"testing"

	networkingv1 "k8s.io/api/networking/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	gatewayv1alpha1 "github.com/hanzoai/hanzo-operator/api/v1alpha1"
)

func TestRenderConfigBasic(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{
				Name:      "team-web",
				Namespace: "team",
			},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "hanzo.team",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{
										Path: "/",
										Backend: networkingv1.IngressBackend{
											Service: &networkingv1.IngressServiceBackend{
												Name: "team-nginx",
												Port: networkingv1.ServiceBackendPort{Number: 80},
											},
										},
									},
								},
							},
						},
					},
				},
			},
		},
	}

	result, err := renderConfig(ingresses, nil, nil)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	if len(result.Config.Routes) != 1 {
		t.Fatalf("expected 1 route, got %d", len(result.Config.Routes))
	}

	route := result.Config.Routes[0]
	if route.Host != "hanzo.team" {
		t.Errorf("expected host hanzo.team, got %s", route.Host)
	}
	if len(route.Backends) != 1 {
		t.Fatalf("expected 1 backend, got %d", len(route.Backends))
	}
	if route.Backends[0].URL != "http://team-nginx.team.svc.cluster.local:80" {
		t.Errorf("unexpected backend URL: %s", route.Backends[0].URL)
	}
	if route.Backends[0].Path != "/" {
		t.Errorf("expected path /, got %s", route.Backends[0].Path)
	}

	// Verify JSON output is valid.
	var parsed GatewayIngressConfig
	if err := json.Unmarshal(result.JSON, &parsed); err != nil {
		t.Fatalf("invalid JSON output: %v", err)
	}
	if parsed.Listen != ":8080" {
		t.Errorf("expected listen :8080, got %s", parsed.Listen)
	}
	if parsed.HealthPath != "/__health" {
		t.Errorf("expected health_path /__health, got %s", parsed.HealthPath)
	}

	// Hash should be non-empty and deterministic.
	if result.Hash == "" {
		t.Error("hash should not be empty")
	}
	result2, _ := renderConfig(ingresses, nil, nil)
	if result.Hash != result2.Hash {
		t.Error("hash should be deterministic")
	}
}

func TestRenderConfigSkipsDisabled(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{
				Name:      "disabled-ing",
				Namespace: "default",
				Annotations: map[string]string{
					"gateway.hanzo.ai/enabled": "false",
				},
			},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "example.com",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{
										Path: "/",
										Backend: networkingv1.IngressBackend{
											Service: &networkingv1.IngressServiceBackend{
												Name: "svc",
												Port: networkingv1.ServiceBackendPort{Number: 80},
											},
										},
									},
								},
							},
						},
					},
				},
			},
		},
	}

	result, err := renderConfig(ingresses, nil, nil)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	if len(result.Config.Routes) != 0 {
		t.Errorf("expected 0 routes for disabled ingress, got %d", len(result.Config.Routes))
	}
	if len(result.Skipped) != 1 {
		t.Errorf("expected 1 skipped, got %d", len(result.Skipped))
	}
	if result.Skipped[0] != "default/disabled-ing" {
		t.Errorf("unexpected skipped entry: %s", result.Skipped[0])
	}
}

func TestRenderConfigConflictDetection(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{Name: "alpha", Namespace: "default"},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "api.hanzo.ai",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{
										Path: "/v1",
										Backend: networkingv1.IngressBackend{
											Service: &networkingv1.IngressServiceBackend{
												Name: "api-v1",
												Port: networkingv1.ServiceBackendPort{Number: 8080},
											},
										},
									},
								},
							},
						},
					},
				},
			},
		},
		{
			ObjectMeta: metav1.ObjectMeta{Name: "beta", Namespace: "default"},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "api.hanzo.ai",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{
										Path: "/v1",
										Backend: networkingv1.IngressBackend{
											Service: &networkingv1.IngressServiceBackend{
												Name: "api-v1-new",
												Port: networkingv1.ServiceBackendPort{Number: 8080},
											},
										},
									},
								},
							},
						},
					},
				},
			},
		},
	}

	result, err := renderConfig(ingresses, nil, nil)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	if len(result.Conflicts) != 1 {
		t.Fatalf("expected 1 conflict, got %d", len(result.Conflicts))
	}

	conflict := result.Conflicts[0]
	if conflict.Host != "api.hanzo.ai" {
		t.Errorf("conflict host: expected api.hanzo.ai, got %s", conflict.Host)
	}
	if conflict.Path != "/v1" {
		t.Errorf("conflict path: expected /v1, got %s", conflict.Path)
	}
	// Winner should be alpha (alphabetically first source: default/alpha < default/beta).
	if conflict.Winner != "http://api-v1.default.svc.cluster.local:8080" {
		t.Errorf("unexpected winner: %s", conflict.Winner)
	}

	// Only one route entry despite conflict.
	if len(result.Config.Routes) != 1 {
		t.Fatalf("expected 1 host route, got %d", len(result.Config.Routes))
	}
	if len(result.Config.Routes[0].Backends) != 1 {
		t.Errorf("expected 1 backend (winner), got %d", len(result.Config.Routes[0].Backends))
	}
}

func TestRenderConfigSortOrder(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{Name: "multi", Namespace: "ns"},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "b.example.com",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{Path: "/", Backend: svcBackend("svc-b-root", 80)},
									{Path: "/api/v1", Backend: svcBackend("svc-b-api", 80)},
									{Path: "/api", Backend: svcBackend("svc-b-api-short", 80)},
								},
							},
						},
					},
					{
						Host: "a.example.com",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{Path: "/", Backend: svcBackend("svc-a", 80)},
								},
							},
						},
					},
				},
			},
		},
	}

	result, err := renderConfig(ingresses, nil, nil)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	if len(result.Config.Routes) != 2 {
		t.Fatalf("expected 2 host routes, got %d", len(result.Config.Routes))
	}

	// Hosts sorted alphabetically.
	if result.Config.Routes[0].Host != "a.example.com" {
		t.Errorf("first host should be a.example.com, got %s", result.Config.Routes[0].Host)
	}
	if result.Config.Routes[1].Host != "b.example.com" {
		t.Errorf("second host should be b.example.com, got %s", result.Config.Routes[1].Host)
	}

	// Paths sorted by length descending.
	bBackends := result.Config.Routes[1].Backends
	if len(bBackends) != 3 {
		t.Fatalf("expected 3 backends for b.example.com, got %d", len(bBackends))
	}
	if bBackends[0].Path != "/api/v1" {
		t.Errorf("first path should be /api/v1 (longest), got %s", bBackends[0].Path)
	}
	if bBackends[1].Path != "/api" {
		t.Errorf("second path should be /api, got %s", bBackends[1].Path)
	}
	if bBackends[2].Path != "/" {
		t.Errorf("third path should be / (shortest), got %s", bBackends[2].Path)
	}
}

func TestRenderConfigAnnotations(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{
				Name:      "annotated",
				Namespace: "default",
				Annotations: map[string]string{
					"gateway.hanzo.ai/methods": "GET,POST",
					"gateway.hanzo.ai/timeout": "5s",
				},
			},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "api.hanzo.ai",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{Path: "/", Backend: svcBackend("api", 8080)},
								},
							},
						},
					},
				},
			},
		},
	}

	result, err := renderConfig(ingresses, nil, nil)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	backend := result.Config.Routes[0].Backends[0]
	if len(backend.Methods) != 2 || backend.Methods[0] != "GET" || backend.Methods[1] != "POST" {
		t.Errorf("unexpected methods: %v", backend.Methods)
	}
	if backend.Timeout != "5s" {
		t.Errorf("expected timeout 5s, got %s", backend.Timeout)
	}
}

func TestRenderConfigLabelSelector(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{
				Name:      "matched",
				Namespace: "default",
				Labels:    map[string]string{"gateway.hanzo.ai/enabled": "true"},
			},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "matched.example.com",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{Path: "/", Backend: svcBackend("matched-svc", 80)},
								},
							},
						},
					},
				},
			},
		},
		{
			ObjectMeta: metav1.ObjectMeta{
				Name:      "unmatched",
				Namespace: "default",
			},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "unmatched.example.com",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{Path: "/", Backend: svcBackend("unmatched-svc", 80)},
								},
							},
						},
					},
				},
			},
		},
	}

	selector := &gatewayv1alpha1.IngressSelector{
		MatchLabels: map[string]string{"gateway.hanzo.ai/enabled": "true"},
	}

	result, err := renderConfig(ingresses, nil, selector)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	if len(result.Config.Routes) != 1 {
		t.Fatalf("expected 1 route (matched only), got %d", len(result.Config.Routes))
	}
	if result.Config.Routes[0].Host != "matched.example.com" {
		t.Errorf("expected matched.example.com, got %s", result.Config.Routes[0].Host)
	}
	if len(result.Skipped) != 1 || result.Skipped[0] != "default/unmatched" {
		t.Errorf("expected unmatched to be skipped, got %v", result.Skipped)
	}
}

func TestRenderConfigDefaults(t *testing.T) {
	ingresses := []networkingv1.Ingress{
		{
			ObjectMeta: metav1.ObjectMeta{Name: "svc", Namespace: "prod"},
			Spec: networkingv1.IngressSpec{
				Rules: []networkingv1.IngressRule{
					{
						Host: "secure.hanzo.ai",
						IngressRuleValue: networkingv1.IngressRuleValue{
							HTTP: &networkingv1.HTTPIngressRuleValue{
								Paths: []networkingv1.HTTPIngressPath{
									{Path: "/", Backend: svcBackend("secure-svc", 443)},
								},
							},
						},
					},
				},
			},
		},
	}

	defaults := &gatewayv1alpha1.DefaultsSpec{
		BackendScheme: "https",
		Timeout:       "60s",
	}

	result, err := renderConfig(ingresses, defaults, nil)
	if err != nil {
		t.Fatalf("renderConfig failed: %v", err)
	}

	backend := result.Config.Routes[0].Backends[0]
	if backend.URL != "https://secure-svc.prod.svc.cluster.local:443" {
		t.Errorf("expected https scheme, got URL: %s", backend.URL)
	}
	if backend.Timeout != "60s" {
		t.Errorf("expected default timeout 60s, got %s", backend.Timeout)
	}
}

// svcBackend is a test helper to create an IngressBackend.
func svcBackend(name string, port int32) networkingv1.IngressBackend {
	return networkingv1.IngressBackend{
		Service: &networkingv1.IngressServiceBackend{
			Name: name,
			Port: networkingv1.ServiceBackendPort{Number: port},
		},
	}
}
