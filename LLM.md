# Hanzo Operator — AI-Friendly Guide

## What
Canonical Kubernetes operator for the Hanzo platform. Rust implementation,
shared by Hanzo, Lux, Zoo, and Osage universes.

One binary. 20 CRD Kinds (8 canonical + 9 unbranded facades + 3 legacy
compat). API group configurable at install time via `--api-group` /
`OPERATOR_API_GROUP` (default `hanzo.ai`).

## Tech Stack
- Rust 1.79+ (stable). edition = 2021.
- kube-rs 0.87 (CustomResource derive + runtime::Controller).
- k8s-openapi 0.20 (v1_28 feature).
- Tokio multi-threaded runtime.
- Image: `ghcr.io/hanzoai/operator:vX.Y.Z` (semver only, no `:latest`).
- Runs in `hanzo-operator-system` namespace.

## CRD Kinds (20 total)

### Canonical (v1)
| Kind        | Short  | Materializes |
|-------------|--------|--------------|
| Service     | hsvc   | Deployment + Service + Ingress + HPA + PDB + NetworkPolicy + KMSSecret |
| Datastore   | hds    | StatefulSet + ClusterIP + headless Service + PVC + KMSSecret |
| Gateway     | hgw    | Deployment + Service + ConfigMap (krakend.json) + Ingress |
| MPC         | hmpc   | StatefulSet + headless + ClusterIP Service |
| Network     | hnet   | StatefulSet (validators) + Services + PVC |
| Ingress     | hing   | Multiple Ingress resources with cert-manager TLS |
| DNS         | hdns   | Deployment + Service (CoreDNS) |
| BaseApp     | bapp   | StatefulSet + headless + ClusterIP Service (Quasar writer election) |

### Facades (v1) — delegate to Service/Datastore
| Kind    | Short | Inner |
|---------|-------|-------|
| SQL     | sql   | Datastore (type=postgresql) |
| KV      | kv    | Datastore (type=valkey) |
| DocDB   | docdb | Datastore (type=docdb) |
| S3      | s3    | Datastore (type=minio) |
| IAM     | iam   | Service |
| KMS     | kms   | Service |
| LLM     | llm   | Service |
| Indexer | idx   | Service |
| Explorer| exp   | Service |

### Network sub-resources (v1) — NoOp stubs (Network handles materialization)
| Kind      | Short  |
|-----------|--------|
| Chain     | chain  |
| Subnet    | subnet |
| Validator | val    |

### Legacy compat (v1alpha1) — delegate to v1 reconcilers
| Kind            | Inner            |
|-----------------|------------------|
| HanzoService    | Service          |
| HanzoDatastore  | Datastore        |
| HanzoDNS        | DNS              |

Existing CRs at `~/work/hanzo/universe/infra/k8s/hanzo-operator/crs/*.yaml`
keep working unchanged through the compat aliases.

## Layout
```
src/
  main.rs           Entrypoint — clap args, leader election, controller spawn.
  lib.rs            Library facade.
  crd.rs            All 20 CRD types.
  crd_types.rs      JsonSchema wrappers for k8s-openapi types.
  manifests.rs      Pure K8s object builders.
  apply.rs          Server-side apply (typed + DynamicObject).
  api_group.rs      Runtime API-group resolution.
  controllers/      One module per Kind.
    service.rs, datastore.rs, gateway.rs, mpc.rs, network.rs,
    ingress.rs, dns.rs, baseapp.rs, compat.rs
  core/             Absorbed from former hanzoai/operator-core repo.
    error.rs, leader.rs, iam_admin.rs, secret.rs, status.rs, reconciler.rs
  bin/
    generate_crd_yaml.rs  CRD YAML generator with --api-group rewriter.
k8s/crds/           Pre-rendered CRD YAML per universe.
```

## Critical invariant
`spec.env`, `spec.volumes`, `spec.volumeMounts` MUST be honored on the
generated Deployment. The gateway 503 root cause (May 2026) was the
legacy Go operator silently dropping these. Tests assert the round-trip:

```bash
cargo test --lib controllers::service::tests
# env_is_carried_to_main_container ... ok
# volume_mounts_are_carried_to_main_container ... ok
# deployment_carries_volumes ... ok
```

## API group rebinding (runtime configurable)
kube-rs's `CustomResource` derive bakes the API group at compile time, so
the binary's compile-time default is `hanzo.ai`. To deploy under another
universe's group, generate the CRD YAML with the rewriter:

```bash
generate-crd-yaml --api-group lux.cloud  --out k8s/crds/all-lux.cloud.yaml
generate-crd-yaml --api-group zoo.cloud  --out k8s/crds/all-zoo.cloud.yaml
generate-crd-yaml --api-group osage.cloud --out k8s/crds/all-osage.cloud.yaml
```

The operator itself accepts `--api-group X.Y` or `OPERATOR_API_GROUP=X.Y`
and uses the resolved group when building owner references and dynamic
KMSSecret CR references.

## Build / Test / Lint
```bash
cargo build --release
cargo test --lib                      # 35 unit tests
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

CI: `.github/workflows/publish.yml` uses
`hanzoai/.github/.github/workflows/docker-build.yml@main`. Tags `v*`
publish `ghcr.io/hanzoai/operator:vX.Y.Z` for linux/amd64 + linux/arm64
(arm64 falls back to QEMU if the ARC scale set is paused per LLM.md
2026-04-27).

## Predecessor
The Go implementation lives on the `legacy/go-impl-before-rust-port`
branch here. The maintained Go operator is its own repo at
`luxfi/operator` (web3 canonical); this Rust impl is web2 canonical.
Both target full feature parity over a shared CRD wire contract.

The shared reconciler primitives in `src/core/` are also published as
the standalone `hanzoai/operator-core` crate, which `zooai/operator`
consumes at git tag `v0.1.0`. Keep `src/core/` and that crate in sync.

## Rules
- ALWAYS use `cargo` not `make` for Rust workflows.
- NEVER set `:latest`, `:main`, `:dev`. Pin `vX.Y.Z` per semver-only
  policy (hanzo CLAUDE.md 2026-04-30).
- Field-for-field wire compatibility with legacy Go types — CRs in the
  cluster MUST NOT need editing when the operator binary is replaced.
- Honor `spec.env/volumes/volumeMounts` — the load-bearing assertion.
- Out of scope this session: rolling out to the cluster. The legacy
  Go operator stays in production until coordinated cutover.

## v0.3.3 — facade controllers (2026-05-19)
The 9 facade Kinds (SQL, KV, DocDB, S3, IAM, KMS, LLM, Indexer, Explorer)
were defined in `src/crd.rs` but orphaned with no controllers between
v0.3.0 and v0.3.2 — creating a CR was a no-op. v0.3.3 ships one
controller per Kind under `src/controllers/`:

- Service-backed facades (`iam.rs`, `kms.rs`, `llm.rs`, `indexer.rs`,
  `explorer.rs`) follow the `queue.rs` template byte-for-byte: unwrap
  `cr.spec.0` (newtype over `ServiceSpec`) and call
  `service::reconcile_service_inner_pub`.
- Datastore-backed facades (`sql.rs`, `kv.rs`, `docdb.rs`, `s3.rs`)
  unwrap the inner `DatastoreSpec` and **force** `spec.type` to the
  canonical value (`postgresql` / `valkey` / `docdb` / `minio`) before
  delegating to `datastore::reconcile_datastore_inner_pub`. This makes
  the facade kind authoritative — a `SQL` CR cannot accidentally
  materialize as a Valkey or MinIO datastore even if the user sets
  `spec.type` to something else.

Each controller is ~75 LoC with one smoke test asserting newtype
unwrap (service facades) or type override (datastore facades).
All 9 are wired into `main.rs`'s `tokio::join!` so they spin up
alongside the canonical Kinds when the leader is elected.

Test count: 35 → 44 (9 new smoke tests, 0 regressions).
