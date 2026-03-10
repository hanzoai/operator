// Package rollout implements progressive deployments across environments
// (devnet -> testnet -> mainnet) driven by universe manifests.
//
// Originally developed for liquidityio/operator; adopted as a subcommand of
// hanzo-operator so a single binary handles both CRD reconciliation and
// operator-driven rollouts.
package rollout

import (
	"fmt"
	"os"
	"path/filepath"
	"regexp"

	"gopkg.in/yaml.v3"
)

// Env represents a deployment environment.
type Env string

const (
	Devnet  Env = "devnet"
	Testnet Env = "testnet"
	Mainnet Env = "mainnet"
)

// PromotionOrder is the progressive deploy sequence.
var PromotionOrder = []Env{Devnet, Testnet, Mainnet}

// Manifest is the top-level universe manifest.
type Manifest struct {
	Version  string             `yaml:"version"`
	Context  string             `yaml:"context"`
	Project  string             `yaml:"project"`
	Domain   string             `yaml:"domain"`
	Replicas int                `yaml:"replicas"`
	Services map[string]Service `yaml:"services"`
}

// Service is a single service entry in the manifest.
type Service struct {
	Image     string `yaml:"image"`
	Namespace string `yaml:"namespace"`
	Kind      string `yaml:"kind"` // "deployment" or "statefulset"
}

// K8sKind returns the kubectl-friendly resource kind.
func (s Service) K8sKind() string {
	switch s.Kind {
	case "statefulset":
		return "sts"
	default:
		return "deploy"
	}
}

// IsBaseService returns true for services that use Base (health at /v1/base/health).
func IsBaseService(name string) bool {
	switch name {
	case "ats", "bd", "ta":
		return true
	default:
		return false
	}
}

// Tag extracts the image tag (everything after the last colon).
func (s Service) Tag() string {
	for i := len(s.Image) - 1; i >= 0; i-- {
		if s.Image[i] == ':' {
			return s.Image[i+1:]
		}
	}
	return ""
}

var semverRe = regexp.MustCompile(`^v\d+\.\d+\.\d+$`)

// ValidateTag checks that the image tag is semver.
func (s Service) ValidateTag(name string) error {
	tag := s.Tag()
	if !semverRe.MatchString(tag) {
		return fmt.Errorf("service %s has non-semver tag %q (must be vX.Y.Z)", name, tag)
	}
	return nil
}

// envFile maps env name to manifest filename.
func envFile(env Env) string {
	switch env {
	case Devnet:
		return "dev.yml"
	case Testnet:
		return "test.yml"
	case Mainnet:
		return "main.yml"
	default:
		return ""
	}
}

// LoadManifest loads a manifest for a given environment.
func LoadManifest(dir string, env Env) (*Manifest, error) {
	path := filepath.Join(dir, envFile(env))
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read manifest %s: %w", path, err)
	}
	var m Manifest
	if err := yaml.Unmarshal(data, &m); err != nil {
		return nil, fmt.Errorf("parse manifest %s: %w", path, err)
	}
	return &m, nil
}

// LoadAll loads manifests for all environments.
func LoadAll(dir string) (map[Env]*Manifest, error) {
	result := make(map[Env]*Manifest, 3)
	for _, env := range PromotionOrder {
		m, err := LoadManifest(dir, env)
		if err != nil {
			return nil, err
		}
		result[env] = m
	}
	return result, nil
}
