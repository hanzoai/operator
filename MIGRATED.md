# MIGRATED — canonical Rust impl now at luxfi/operator/rust/

As of 2026-05-29, the canonical home of the Hanzo operator Rust
implementation is **luxfi/operator/rust/**, in the polyglot
`luxfi/operator` repo alongside the Go implementation under `go/`.

```
github.com/luxfi/operator/rust/    Rust impl (this repo's HEAD at f50a18d)
github.com/luxfi/operator/go/      Go impl (controller-runtime)
github.com/luxfi/operator/spec/    Shared CRD wire contract
github.com/luxfi/operator/config/  Shared Kustomize install
```

## Why

- One polyglot repo for one operator protocol — go/ and rust/ sit side
  by side, both honor the shared `spec/` wire contract.
- Mirrors the `luxfi/consensus` `pkg/<lang>/` pattern.
- Eliminates the dual-canonical state where hanzoai/operator (Rust) and
  luxfi/operator (Go) drifted from a single CRD contract.

## What stays alive in this repo

The `main` branch is kept readable as a tag-frozen archive so that:

- `ghcr.io/hanzoai/operator:sha-81a43b0` and earlier vX.Y.Z tags
  remain reachable for in-cluster deployments referencing them.
- The Go pseudo-version
  `github.com/hanzoai/operator v0.2.3-0.20260518164038-81a43b0f8156`
  used by `hanzoai/superbase` and `hanzoai/agents/control-plane`
  (`api/v1alpha1` Go types from the legacy/go-impl-before-rust-port
  branch) continues to resolve.

No new feature work happens here. PRs land at
`luxfi/operator/rust/` instead.

## Migration plan for downstream consumers

| Consumer | Today | After migration |
|----------|-------|-----------------|
| `hanzoai/universe/infra/k8s/operator/deployment.yaml` | `ghcr.io/hanzoai/operator:<sha>` | `ghcr.io/luxfi/operator-rust:vX.Y.Z` (when ready) or `ghcr.io/luxfi/operator:vX.Y.Z` for the Go impl |
| `hanzoai/superbase` go.mod | `github.com/hanzoai/operator v0.2.3-0...` | Repointed to `github.com/luxfi/operator/go` v1alpha1 types (api package extracted) |
| `hanzoai/agents/control-plane` go.mod | same | same |
| Downstream operator forks (Cargo.toml operator-core dep) | `hanzo-operator-core` git tag | unaffected — operator-core lives separately (see below) |

## operator-core relationship

The shared reconciler crate `hanzo-operator-core` (formerly at
`github.com/hanzoai/operator-core`) was absorbed into `src/core/` of
this Rust impl during the Go → Rust port. That absorbed `core/` module
moves with the Rust source to luxfi/operator/rust/src/core/.

The standalone `github.com/hanzoai/operator-core` repo at
`~/work/hanzo/operator-core` still exists and is still consumed by
downstream operator forks. Until they migrate off, the standalone crate
stays alive. No change required for this migration.

## Branches

- `main` — frozen at f50a18d after the move; feature-frozen, security
  patches only.
- `legacy/go-impl-before-rust-port` — pre-existing branch with the
  original Go scaffold. Pulled by superbase and agents-control-plane
  via Go pseudo-version. Stays alive.
- `legacy/rust-impl-before-go-rewrite` — N/A (does not exist in
  this repo; that name lives only in luxfi/operator referring to its
  pre-Go-rewrite Rust impl).

See `~/work/lux/operator/README.md` for the new polyglot home.
