// Command rollout performs progressive deployments across Hanzo clusters.
//
// It reads universe manifests, compares desired vs running images, and deploys
// progressively: devnet -> health check -> testnet -> health check -> mainnet.
// On health failure it rolls back to the previous image and blocks promotion.
//
// This package is also invoked as a subcommand of hanzo-operator (see cmd/main.go):
//
//	hanzo-operator rollout --service ats --env devnet
//
// Standalone usage:
//
//	rollout                     # sync all services across all envs
//	rollout --service ats       # sync only ats
//	rollout --env devnet        # sync only devnet (no promotion)
//	rollout --dry-run           # show what would change
package main

import (
	"os"

	"github.com/hanzoai/operator/cmd/rollout/cli"
)

func main() {
	os.Exit(cli.Run(os.Args[1:]))
}
