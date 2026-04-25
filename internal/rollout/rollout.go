// Package rollout re-exports github.com/hanzoai/rollout so existing import
// sites in this repo do not need to change. The canonical implementation
// lives in ~/work/hanzo/rollout (module github.com/hanzoai/rollout) and is
// shared with ~/work/liquidity/operator.
package rollout

import (
	"context"

	shared "github.com/hanzoai/rollout"
)

// Config is the rollout configuration.
type Config = shared.Config

// ServiceDiff is a per-service version difference.
type ServiceDiff = shared.ServiceDiff

// Manifest is a service manifest for a single environment.
type Manifest = shared.Manifest

// Service is one entry in a manifest.
type Service = shared.Service

// Env is a deployment environment label.
type Env = shared.Env

// Environment constants.
const (
	Devnet  = shared.Devnet
	Testnet = shared.Testnet
	Mainnet = shared.Mainnet
)

// PromotionOrder is the canonical devnet -> testnet -> mainnet sequence.
var PromotionOrder = shared.PromotionOrder

// Run executes the progressive rollout.
func Run(ctx context.Context, cfg Config) error { return shared.Run(ctx, cfg) }

// LoadManifest reads the manifest for a single env.
func LoadManifest(dir string, env Env) (*Manifest, error) {
	return shared.LoadManifest(dir, env)
}

// LoadAll reads dev.yml/test.yml/main.yml from dir.
func LoadAll(dir string) (map[Env]*Manifest, error) { return shared.LoadAll(dir) }

// IsBaseService reports whether name uses the Base health endpoint.
func IsBaseService(name string) bool { return shared.IsBaseService(name) }

// HealthPath returns the HTTP path probed for a service.
func HealthPath(name string) string { return shared.HealthPath(name) }

// HealthPort returns the container port probed for a service.
func HealthPort(name string) int { return shared.HealthPort(name) }
