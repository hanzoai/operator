package rollout

import (
	"os"
	"path/filepath"
	"testing"
)

const testManifest = `version: "1"
context: gke_hanzo-devnet_us-central1_dev
project: hanzo-devnet
domain: dev.hanzo.ai
replicas: 1

services:
  ats:
    image: ghcr.io/hanzoai/ats:v1.19.4
    namespace: hanzo
    kind: statefulset
  bd:
    image: ghcr.io/hanzoai/bd:v1.3.0
    namespace: hanzo
    kind: statefulset
  gateway:
    image: ghcr.io/hanzoai/gateway:v1.0.2
    namespace: hanzo
    kind: deployment
  exchange:
    image: ghcr.io/hanzoai/exchange:v1.2.26
    namespace: hanzo
    kind: deployment
`

func writeTestManifests(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	for _, f := range []string{"dev.yml", "test.yml", "main.yml"} {
		if err := os.WriteFile(filepath.Join(dir, f), []byte(testManifest), 0644); err != nil {
			t.Fatal(err)
		}
	}
	return dir
}

func TestLoadAll(t *testing.T) {
	dir := writeTestManifests(t)

	manifests, err := LoadAll(dir)
	if err != nil {
		t.Fatal(err)
	}

	if len(manifests) != 3 {
		t.Errorf("expected 3 manifests, got %d", len(manifests))
	}

	for _, env := range PromotionOrder {
		m, ok := manifests[env]
		if !ok {
			t.Errorf("missing manifest for %s", env)
			continue
		}
		if len(m.Services) != 4 {
			t.Errorf("%s: expected 4 services, got %d", env, len(m.Services))
		}
	}
}

func TestValidateAllSemver(t *testing.T) {
	dir := writeTestManifests(t)

	manifests, err := LoadAll(dir)
	if err != nil {
		t.Fatal(err)
	}

	for env, m := range manifests {
		for name, svc := range m.Services {
			if err := svc.ValidateTag(name); err != nil {
				t.Errorf("%s/%s: %v", env, name, err)
			}
		}
	}
}

func TestValidateRejectsBadTag(t *testing.T) {
	dir := t.TempDir()
	bad := `version: "1"
context: test
project: test
domain: test
replicas: 1

services:
  ats:
    image: ghcr.io/hanzoai/ats:dev
    namespace: hanzo
    kind: statefulset
`
	for _, f := range []string{"dev.yml", "test.yml", "main.yml"} {
		if err := os.WriteFile(filepath.Join(dir, f), []byte(bad), 0644); err != nil {
			t.Fatal(err)
		}
	}

	manifests, err := LoadAll(dir)
	if err != nil {
		t.Fatal(err)
	}

	for _, m := range manifests {
		for name, svc := range m.Services {
			if err := svc.ValidateTag(name); err == nil {
				t.Errorf("expected error for non-semver tag for %s", name)
			}
		}
	}
}

func TestPromotionOrder(t *testing.T) {
	if len(PromotionOrder) != 3 {
		t.Fatalf("expected 3 envs in promotion order, got %d", len(PromotionOrder))
	}
	if PromotionOrder[0] != Devnet {
		t.Errorf("first env should be devnet, got %s", PromotionOrder[0])
	}
	if PromotionOrder[1] != Testnet {
		t.Errorf("second env should be testnet, got %s", PromotionOrder[1])
	}
	if PromotionOrder[2] != Mainnet {
		t.Errorf("third env should be mainnet, got %s", PromotionOrder[2])
	}
}
