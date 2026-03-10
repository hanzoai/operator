package rollout

import (
	"context"
	"fmt"
	"log/slog"
	"time"
)

// Config holds rollout configuration.
type Config struct {
	ManifestDir    string
	Service        string // empty = all services
	Env            string // empty = progressive (devnet->testnet->mainnet)
	DryRun         bool
	HealthTimeout  time.Duration
	RolloutTimeout time.Duration
	HealthRetries  int
}

// ServiceDiff represents a version difference for one service.
type ServiceDiff struct {
	Name         string
	Namespace    string
	Kind         string
	DesiredImage string
	CurrentImage string
}

// Run executes the progressive rollout.
func Run(ctx context.Context, cfg Config) error {
	manifests, err := LoadAll(cfg.ManifestDir)
	if err != nil {
		return err
	}

	// Validate all tags are semver
	for env, m := range manifests {
		for name, svc := range m.Services {
			if err := svc.ValidateTag(name); err != nil {
				return fmt.Errorf("%s: %w", env, err)
			}
		}
	}

	// Determine which envs to process
	envs := PromotionOrder
	if cfg.Env != "" {
		e := Env(cfg.Env)
		if _, ok := manifests[e]; !ok {
			return fmt.Errorf("unknown env %q", cfg.Env)
		}
		envs = []Env{e}
	}

	for _, env := range envs {
		m := manifests[env]
		slog.Info("processing environment", "env", env, "context", m.Context)

		diffs, err := computeDiffs(ctx, m, cfg.Service)
		if err != nil {
			return fmt.Errorf("%s: compute diffs: %w", env, err)
		}

		if len(diffs) == 0 {
			slog.Info("all services up to date", "env", env)
			continue
		}

		for _, d := range diffs {
			slog.Info("version drift detected",
				"env", env,
				"service", d.Name,
				"current", d.CurrentImage,
				"desired", d.DesiredImage,
			)
		}

		if cfg.DryRun {
			printDiffs(env, diffs)
			continue
		}

		// Deploy each service, health check, rollback on failure
		for _, d := range diffs {
			if err := deployOne(ctx, cfg, env, m.Context, d); err != nil {
				return fmt.Errorf("%s/%s: %w", env, d.Name, err)
			}
		}

		slog.Info("environment synced", "env", env)
	}

	slog.Info("rollout complete")
	return nil
}

// computeDiffs compares manifest desired state with cluster running state.
func computeDiffs(ctx context.Context, m *Manifest, filterService string) ([]ServiceDiff, error) {
	var diffs []ServiceDiff

	for name, svc := range m.Services {
		if filterService != "" && name != filterService {
			continue
		}

		current, err := GetCurrentImage(ctx, m.Context, svc.Namespace, svc.K8sKind(), name)
		if err != nil {
			// Service might not exist yet — treat as needing deploy
			slog.Warn("cannot get current image", "service", name, "error", err)
			current = "MISSING"
		}

		if current != svc.Image {
			diffs = append(diffs, ServiceDiff{
				Name:         name,
				Namespace:    svc.Namespace,
				Kind:         svc.K8sKind(),
				DesiredImage: svc.Image,
				CurrentImage: current,
			})
		}
	}

	return diffs, nil
}

// deployOne deploys a single service with health check and rollback.
func deployOne(ctx context.Context, cfg Config, env Env, kubeCtx string, diff ServiceDiff) error {
	slog.Info("deploying",
		"env", env,
		"service", diff.Name,
		"from", diff.CurrentImage,
		"to", diff.DesiredImage,
	)

	// Set the new image
	if err := SetImage(ctx, kubeCtx, diff.Namespace, diff.Kind, diff.Name, diff.DesiredImage); err != nil {
		return fmt.Errorf("set image: %w", err)
	}

	// Wait for rollout to finish
	slog.Info("waiting for rollout", "service", diff.Name, "timeout", cfg.RolloutTimeout)
	if err := WaitRollout(ctx, kubeCtx, diff.Namespace, diff.Kind, diff.Name, cfg.RolloutTimeout); err != nil {
		slog.Error("rollout did not complete", "service", diff.Name, "error", err)
		return rollback(ctx, kubeCtx, diff, err)
	}

	// Health check
	slog.Info("running health check", "service", diff.Name, "retries", cfg.HealthRetries)
	if err := CheckHealth(ctx, kubeCtx, diff.Namespace, diff.Kind, diff.Name, cfg.HealthTimeout, cfg.HealthRetries); err != nil {
		slog.Error("health check failed", "service", diff.Name, "error", err)
		return rollback(ctx, kubeCtx, diff, err)
	}

	slog.Info("deploy succeeded", "env", env, "service", diff.Name, "image", diff.DesiredImage)
	return nil
}

// rollback reverts a service to its previous image.
func rollback(ctx context.Context, kubeCtx string, diff ServiceDiff, cause error) error {
	if diff.CurrentImage == "" || diff.CurrentImage == "MISSING" {
		return fmt.Errorf("cannot rollback %s (no previous image): %w", diff.Name, cause)
	}

	slog.Warn("rolling back",
		"service", diff.Name,
		"to", diff.CurrentImage,
		"reason", cause.Error(),
	)

	if err := SetImage(ctx, kubeCtx, diff.Namespace, diff.Kind, diff.Name, diff.CurrentImage); err != nil {
		return fmt.Errorf("rollback set image failed for %s: %w (original error: %v)", diff.Name, err, cause)
	}

	// Wait for rollback to complete
	if err := WaitRollout(ctx, kubeCtx, diff.Namespace, diff.Kind, diff.Name, 120*time.Second); err != nil {
		return fmt.Errorf("rollback rollout failed for %s: %w (original error: %v)", diff.Name, err, cause)
	}

	return fmt.Errorf("rolled back %s to %s: %w", diff.Name, diff.CurrentImage, cause)
}

// printDiffs prints a dry-run summary.
func printDiffs(env Env, diffs []ServiceDiff) {
	fmt.Printf("\n  %s:\n", env)
	for _, d := range diffs {
		curTag := imageTag(d.CurrentImage)
		desTag := imageTag(d.DesiredImage)
		fmt.Printf("    %-15s %-12s %-10s -> %-10s\n", d.Name, d.Namespace, curTag, desTag)
	}
}

func imageTag(image string) string {
	for i := len(image) - 1; i >= 0; i-- {
		if image[i] == ':' {
			return image[i+1:]
		}
	}
	return image
}
