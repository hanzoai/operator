package controllers

import (
	"testing"

	gatewayv1alpha1 "github.com/hanzoai/hanzo-operator/api/v1alpha1"
)

func TestResolveProvider(t *testing.T) {
	tests := []struct {
		name     string
		input    gatewayv1alpha1.IngressProvider
		expected gatewayv1alpha1.IngressProvider
	}{
		{"empty defaults to traefik", "", gatewayv1alpha1.IngressProviderTraefik},
		{"explicit traefik", gatewayv1alpha1.IngressProviderTraefik, gatewayv1alpha1.IngressProviderTraefik},
		{"explicit custom", gatewayv1alpha1.IngressProviderCustom, gatewayv1alpha1.IngressProviderCustom},
		{"unknown defaults to traefik", "nginx", gatewayv1alpha1.IngressProviderTraefik},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			gwc := &gatewayv1alpha1.GatewayConfig{
				Spec: gatewayv1alpha1.GatewayConfigSpec{
					IngressProvider: tc.input,
				},
			}
			got := resolveProvider(gwc)
			if got != tc.expected {
				t.Errorf("resolveProvider(%q) = %q, want %q", tc.input, got, tc.expected)
			}
		})
	}
}
