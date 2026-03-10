package rollout

import (
	"os"
	"path/filepath"
	"testing"
)

func TestServiceTag(t *testing.T) {
	tests := []struct {
		image string
		want  string
	}{
		{"ghcr.io/hanzoai/operator:v1.19.4", "v1.19.4"},
		{"ghcr.io/hanzoai/gateway:v1.0.0", "v1.0.0"},
		{"no-tag", ""},
	}
	for _, tt := range tests {
		s := Service{Image: tt.image}
		if got := s.Tag(); got != tt.want {
			t.Errorf("Tag(%q) = %q, want %q", tt.image, got, tt.want)
		}
	}
}

func TestValidateTag(t *testing.T) {
	good := []string{"v1.0.0", "v1.19.4", "v0.0.1"}
	for _, tag := range good {
		s := Service{Image: "foo:" + tag}
		if err := s.ValidateTag("test"); err != nil {
			t.Errorf("ValidateTag(%q) unexpected error: %v", tag, err)
		}
	}

	bad := []string{"dev", "main", "latest", "abc123", "1.0.0"}
	for _, tag := range bad {
		s := Service{Image: "foo:" + tag}
		if err := s.ValidateTag("test"); err == nil {
			t.Errorf("ValidateTag(%q) expected error, got nil", tag)
		}
	}
}

func TestIsBaseService(t *testing.T) {
	for _, name := range []string{"ats", "bd", "ta"} {
		if !IsBaseService(name) {
			t.Errorf("IsBaseService(%q) = false, want true", name)
		}
	}
	for _, name := range []string{"gateway", "exchange", "superadmin", "kms"} {
		if IsBaseService(name) {
			t.Errorf("IsBaseService(%q) = true, want false", name)
		}
	}
}

func TestK8sKind(t *testing.T) {
	if s := (Service{Kind: "statefulset"}); s.K8sKind() != "sts" {
		t.Errorf("K8sKind(statefulset) = %q, want sts", s.K8sKind())
	}
	if s := (Service{Kind: "deployment"}); s.K8sKind() != "deploy" {
		t.Errorf("K8sKind(deployment) = %q, want deploy", s.K8sKind())
	}
}

func TestLoadManifest(t *testing.T) {
	dir := t.TempDir()

	content := `version: "1"
context: gke_hanzo-devnet_us-central1_dev
project: hanzo-devnet
domain: dev.hanzo.ai
replicas: 1

services:
  ats:
    image: ghcr.io/hanzoai/ats:v1.19.4
    namespace: hanzo
    kind: statefulset
  gateway:
    image: ghcr.io/hanzoai/gateway:v1.0.2
    namespace: hanzo
    kind: deployment
`
	if err := os.WriteFile(filepath.Join(dir, "dev.yml"), []byte(content), 0644); err != nil {
		t.Fatal(err)
	}

	m, err := LoadManifest(dir, Devnet)
	if err != nil {
		t.Fatal(err)
	}

	if m.Context != "gke_hanzo-devnet_us-central1_dev" {
		t.Errorf("context = %q", m.Context)
	}
	if m.Replicas != 1 {
		t.Errorf("replicas = %d", m.Replicas)
	}
	if len(m.Services) != 2 {
		t.Errorf("services count = %d, want 2", len(m.Services))
	}

	ats := m.Services["ats"]
	if ats.Kind != "statefulset" {
		t.Errorf("ats kind = %q", ats.Kind)
	}
	if ats.Tag() != "v1.19.4" {
		t.Errorf("ats tag = %q", ats.Tag())
	}
	if ats.Namespace != "hanzo" {
		t.Errorf("ats namespace = %q", ats.Namespace)
	}
}

func TestHealthPath(t *testing.T) {
	if p := HealthPath("ats"); p != "/v1/base/health" {
		t.Errorf("HealthPath(ats) = %q", p)
	}
	if p := HealthPath("gateway"); p != "/healthz" {
		t.Errorf("HealthPath(gateway) = %q", p)
	}
}

func TestHealthPort(t *testing.T) {
	tests := map[string]int{
		"ats":        8090,
		"bd":         8090,
		"ta":         8090,
		"iam":        8000,
		"kms":        8443,
		"gateway":    8080,
		"exchange":   3000,
		"superadmin": 3000,
	}
	for name, want := range tests {
		if got := HealthPort(name); got != want {
			t.Errorf("HealthPort(%q) = %d, want %d", name, got, want)
		}
	}
}
