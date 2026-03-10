// Package cli implements the rollout command-line interface so both the
// standalone `rollout` binary and the `hanzo-operator rollout` subcommand can
// share flag parsing and execution.
package cli

import (
	"context"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/hanzoai/operator/internal/rollout"
)

// Run parses argv (not including the program name or subcommand) and executes
// the rollout. Returns a process exit code.
func Run(argv []string) int {
	fs := flag.NewFlagSet("rollout", flag.ContinueOnError)

	var (
		manifestDir    string
		service        string
		env            string
		dryRun         bool
		healthTimeout  time.Duration
		rolloutTimeout time.Duration
		healthRetries  int
		logLevel       string
	)

	fs.StringVar(&manifestDir, "manifests", "", "path to universe manifests dir (default: auto-detect)")
	fs.StringVar(&service, "service", "", "deploy only this service (default: all)")
	fs.StringVar(&env, "env", "", "deploy only to this env: devnet|testnet|mainnet (default: progressive)")
	fs.BoolVar(&dryRun, "dry-run", false, "show what would change without applying")
	fs.DurationVar(&healthTimeout, "health-timeout", 60*time.Second, "timeout for health check per service")
	fs.DurationVar(&rolloutTimeout, "rollout-timeout", 120*time.Second, "timeout for rollout to complete")
	fs.IntVar(&healthRetries, "health-retries", 5, "number of health check retries before rollback")
	fs.StringVar(&logLevel, "log-level", "info", "log level: debug|info|warn|error")

	if err := fs.Parse(argv); err != nil {
		return 2
	}

	// Logger
	var level slog.Level
	switch strings.ToLower(logLevel) {
	case "debug":
		level = slog.LevelDebug
	case "warn":
		level = slog.LevelWarn
	case "error":
		level = slog.LevelError
	default:
		level = slog.LevelInfo
	}
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: level})))

	// Auto-detect manifests dir
	if manifestDir == "" {
		candidates := []string{
			"../universe/infra/manifests",
			"../../universe/infra/manifests",
			"../universe/manifests",
			"../../universe/manifests",
			os.Getenv("HOME") + "/work/hanzo/universe/infra/manifests",
			os.Getenv("HOME") + "/work/hanzo/universe/manifests",
			os.Getenv("HOME") + "/work/liquidity/universe/manifests",
		}
		for _, c := range candidates {
			if info, err := os.Stat(c); err == nil && info.IsDir() {
				manifestDir = c
				break
			}
		}
		if manifestDir == "" {
			fmt.Fprintln(os.Stderr, "error: cannot find manifests directory; use --manifests")
			return 1
		}
	}

	cfg := rollout.Config{
		ManifestDir:    manifestDir,
		Service:        service,
		Env:            env,
		DryRun:         dryRun,
		HealthTimeout:  healthTimeout,
		RolloutTimeout: rolloutTimeout,
		HealthRetries:  healthRetries,
	}

	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	if err := rollout.Run(ctx, cfg); err != nil {
		slog.Error("rollout failed", "error", err)
		return 1
	}
	return 0
}
