# operator

Canonical Kubernetes operator for the Hanzo platform — Rust port, used by Hanzo, Lux, Zoo, and Osage universes.

One binary. 20 CRD Kinds. Configurable API group at install time. See [`LLM.md`](./LLM.md) for the agent-friendly overview.

## What it manages

Twenty Kinds at `<api-group>/v1`, with three legacy `v1alpha1` compat aliases for existing CRs in cluster.

| Kind        | Purpose                                                   | Materializes |
|-------------|-----------------------------------------------------------|--------------|
| Service     | Stateless service (most common)                           | Deployment, Service, Ingress, HPA, PDB, NetworkPolicy, KMSSecret |
| Datastore   | Generic stateful service (dispatches on `spec.type`)      | StatefulSet, ClusterIP + headless Service, PVC, KMSSecret |
| SQL / KV / DocDB / S3 | Thin facades over Datastore for each engine     | Same as Datastore |
| Gateway     | KrakenD-based API gateway                                 | Deployment, Service, ConfigMap (krakend.json), Ingress |
| MPC         | Multi-party computation threshold cluster                 | StatefulSet, ClusterIP + headless Service |
| Network     | Blockchain validator network                              | StatefulSet (validators), Services, PVC |
| Ingress     | Multi-domain routing with cert-manager TLS                | Multiple Ingress resources |
| DNS         | Multi-tenant CoreDNS deployment                           | Deployment, Service |
| BaseApp     | hanzoai/base-ha cluster (Quasar-pinned writer)            | StatefulSet, headless + ClusterIP Services |
| IAM / KMS / LLM / Indexer / Explorer | Thin facades over Service          | Same as Service |
| Chain / Subnet / Validator | Sub-resources of Network (NoOp stubs)        | — |
| HanzoService / HanzoDatastore / HanzoDNS | v1alpha1 legacy aliases (compat) | Delegate to canonical reconcilers |

## Critical invariant

`spec.env`, `spec.volumes`, `spec.volumeMounts` MUST be honored on the generated Deployment. The gateway 503 root cause (May 2026) was the legacy Go operator silently dropping these. The Rust port carries tests that fail if any of these fields stop being carried.

```bash
$ cargo test --lib controllers::service::tests
test controllers::service::tests::env_is_carried_to_main_container ... ok
test controllers::service::tests::volume_mounts_are_carried_to_main_container ... ok
test controllers::service::tests::deployment_carries_volumes ... ok
```

## Install

### CRDs

CRD YAMLs are pre-generated for each universe under `k8s/crds/`:

```bash
kubectl apply -f k8s/crds/all-hanzo.ai.yaml      # Hanzo universe
kubectl apply -f k8s/crds/all-lux.cloud.yaml     # Lux universe
kubectl apply -f k8s/crds/all-zoo.cloud.yaml     # Zoo universe
kubectl apply -f k8s/crds/all-osage.cloud.yaml   # Osage universe
```

To generate for another universe:

```bash
cargo run --release --bin generate-crd-yaml -- --api-group your.cloud --out k8s/crds/all-your.cloud.yaml
```

### Operator

```bash
# Defaults to API group `hanzo.ai`.
operator

# Other universes override with --api-group or OPERATOR_API_GROUP:
operator --api-group lux.cloud
OPERATOR_API_GROUP=zoo.cloud operator
```

Container image: `ghcr.io/hanzoai/operator:vX.Y.Z` (amd64 + arm64).

## Build

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI: `.github/workflows/publish.yml` uses the shared
`hanzoai/.github/.github/workflows/docker-build.yml@main` workflow. Tags
`v*` publish to `ghcr.io/hanzoai/operator`.

## Architecture

```
src/
  main.rs             Entrypoint — clap args, leader election, controller spawn.
  lib.rs              Library facade (used by generate-crd-yaml + tests).
  crd.rs              All 20 CRD type definitions.
  crd_types.rs        JsonSchema-friendly wrappers for k8s-openapi types
                      (EnvVar, Volume, Condition, etc.) since those don't
                      derive JsonSchema. Wire-compatible round-trip.
  manifests.rs        Pure builders for Deployment, Service, Ingress, PDB,
                      NetworkPolicy, StatefulSet, PVC template.
  apply.rs            Server-side apply (typed + DynamicObject variants).
  api_group.rs        Runtime API-group resolution.
  controllers/
    service.rs        Service + HanzoService (compat) reconcilers.
    datastore.rs      Datastore + HanzoDatastore (compat) reconcilers.
    gateway.rs        Gateway reconciler.
    mpc.rs            MPC reconciler.
    network.rs        Network reconciler.
    ingress.rs        Ingress reconciler.
    dns.rs            DNS + HanzoDNS (compat) reconcilers.
    baseapp.rs        BaseApp reconciler (Quasar writer election + gateway wiring).
    compat.rs         Unbranded facades (SQL/KV/DocDB/IAM/KMS/LLM/S3/...) and
                      stubs for Chain/Subnet/Validator.
  core/               Absorbed from former hanzoai/operator-core repo.
    error.rs          OperatorError + Result.
    leader.rs         Lease-based leader election.
    iam_admin.rs      IAM admin API client (POST /v1/iam/admin/applications/upsert).
    secret.rs         KMSSecret hijack guard + NUL-byte rejection.
    status.rs         Standard status.conditions helpers.
    reconciler.rs     Retry cadence (clamp_resync etc.).
  bin/
    generate_crd_yaml.rs    CRD YAML generator with --api-group rewriter.
```

## Predecessor

The Go implementation is preserved on the `legacy/go-impl-before-rust-port`
branch. It will not be brought back; the Rust port is canonical going
forward. See the legacy branch for the original kubebuilder scaffold,
controller code, and CRD YAML.

The standalone `hanzoai/operator-core` repo is a tombstone — its code is
absorbed under `src/core/` here. Downstream consumers (`luxfi/operator`,
`zoo/operator`, `a downstream operator`) will update their `Cargo.toml` to
depend on `hanzoai/operator` directly.

## License

BSD-3-Clause.
