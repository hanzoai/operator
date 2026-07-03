# Hanzo Operator — AI-Friendly Guide

## What
Canonical Kubernetes operator for the Hanzo platform. Rust implementation,
shared by Hanzo, Lux, Zoo, and Osage universes.

One binary. 26 CRD Kinds. No compat aliases — the v1 Kinds are the one
way. API group configurable at install time via `--api-group` /
`OPERATOR_API_GROUP` (default `hanzo.ai`).

## Tech Stack
- Rust 1.79+ (stable). edition = 2021.
- kube 4 (CustomResource derive + runtime::Controller).
- k8s-openapi 0.28 (v1_33 feature).
- schemars 1 (JsonSchema derive). jiff (not chrono) for k8s Time.
- Tokio multi-threaded runtime.
- Image: `ghcr.io/hanzoai/operator:vX.Y.Z` (semver only, no `:latest`).
- Runs in `hanzo-operator-system` namespace.

## CRD Kinds (26 total)

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
| Base        | bapp   | StatefulSet + headless + ClusterIP Service (Quasar writer election) |

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

### App-shaped facades (v1)
| Kind          | Short | Inner |
|---------------|-------|-------|
| SPA           | spa   | Service |
| Static        | st    | Service |
| Queue         | q     | Service |
| Observability | o11y  | Service |
| Function      | fn    | Service |

### Network sub-resources (v1) — NoOp stubs (Network handles materialization)
| Kind      | Short  |
|-----------|--------|
| Chain     | chain  |
| Validator | val    |

### Blockchain (v1)
| Kind       | Short | Materializes |
|------------|-------|--------------|
| LuxRuntime | lrt   | StatefulSet (luxd validators) + Services + PVC + CronJob/Jobs |
| NodeFleet  | nf    | StatefulSet + Services (pinned node fleet) |

## Layout
```
src/
  main.rs           Entrypoint — clap args, leader election, controller spawn.
  lib.rs            Library facade.
  crd.rs            All 26 CRD types.
  crd_types.rs      JsonSchema wrappers for k8s-openapi types.
  manifests.rs      Pure K8s object builders.
  apply.rs          Server-side apply (typed + DynamicObject).
  api_group.rs      Runtime API-group resolution.
  controllers/      One module per Kind.
    service.rs, datastore.rs, gateway.rs, mpc.rs, network.rs,
    ingress.rs, dns.rs, base.rs, luxruntime.rs, nodefleet.rs, …
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

## v0.4.1 — Apps-lifecycle DRIVE controller (PR 5 of platform APPS_LIFECYCLE.md)

The "DRIVE" half of the apps lifecycle. It is **not a CRD Kind** — its
reconcile source is the platform `apps` table (one row per
`(org, app, env)`), read over HTTP from `GET /v1/apps` (the single read
authority that projects every row through platform's one `computeDrift`
module). It is the inverse of platform PR 2's running-tag reader: the
reader observes the cluster INTO the table; this controller drives the
table BACK ONTO the cluster.

Files:
- `src/core/apps_client.rs` — read-side: `AppView` DTO (the `/v1/apps`
  wire shape), `list_apps[_for_cluster]` (reqwest + Bearer, mirrors
  `core::iam_admin`), and the two pure predicates the boundary needs:
  `is_semver` (`^v\d+\.\d+\.\d+$`) and `parse_image_ref` (the Rust mirror
  of platform's canonical `parseImageRef`).
- `src/controllers/apps.rs` — the poll loop + the pure policy `decide()`
  (the safety brain) + Deployment patch + rollout wait + k8s Event
  emission.

What it does each sweep: read the rows for THIS operator's cluster, and
for each row where `declared_tag != running_tag`, find the Deployment in
the row's namespace whose container image **repository == `apps.registry`**
(the SAME join key the reader uses — NOT the Deployment name, which the
operator derives from a CR and can differ, e.g. `cloud` → `cloud-api`),
then patch only that container's image to `<registry>:<declared_tag>` and
wait for rollout (`rollout_complete`: observed-generation caught up,
updated==desired==available, no lingering replicas).

### Safety gate (the load-bearing part — DRY-RUN BY DEFAULT)

This controller can roll the whole fleet, so it NEVER patches until FOUR
gates open — three configurable, one absolute:

1. **Master enable** `APPS_CONTROLLER=true` (default off → loop never
   starts; mirrors `KMS_ZAP_CONTROLLER`). First deploy of this binary is
   inert.
2. **Drive mode** `APPS_DRIVE_MODE` ∈ {`off` (default), `dry-run`, `on`}.
   `off`/`dry-run` NEVER patch — they log + emit a `DriveIntended` Event
   describing the patch they WOULD apply. Only `on` can patch.
3. **Per-app allow-list** `APPS_DRIVE_ALLOW` (comma-sep
   `<org>/<app>/<env>` | `<org>/<app>` | `<org>/*` | `*`). Even in `on`
   mode, an app NOT matched is dry-run-reported. So `on` does NOT
   reconcile-and-patch everything — you opt each app in explicitly.
4. **Semver-only at the reconcile boundary** — ABSOLUTE, no config
   overrides it. A `declared_tag` that is not `^v\d+\.\d+\.\d+$` is
   refused (e.g. the kms `multi-issuer` seed, a stray `:main`). The
   `decide()` order checks semver BEFORE mode/allow so dry-run output
   never promises a patch that `on` would refuse.

`decide(mode, allow, app) -> Skip | Report | Patch` is pure and the unit
of test (no cluster/platform needed). `decide_floating_..._even_when_on_and_allowed`
and `decide_on_but_not_allowlisted_only_reports` lock the two
fleet-protecting properties.

### Enabling real drive (the deploy steps)

The controller is OFF in every existing manifest (the env vars are
unset). To turn it on, on the operator Deployment in the universe
manifests (`hanzoai/universe` operator deployment) set:

```
APPS_CONTROLLER=true                 # master enable
APPS_PLATFORM_URL=http://platform.<ns>.svc.cluster.local:3000   # /v1/apps base
APPS_SERVICE_TOKEN=<token>           # (or PLATFORM_SERVICE_TOKEN / HANZO_API_KEY) — from a KMS-synced Secret
APPS_CLUSTER=hanzo-k8s              # which cluster's rows to drive (default hanzo-k8s)
# --- still dry-run until BOTH of these are set: ---
APPS_DRIVE_MODE=on                   # off|dry-run|on  (default off)
APPS_DRIVE_ALLOW=hanzoai/iam/test  # start with ONE app, widen deliberately
# optional: APPS_ORG_ID=<org>, APPS_POLL_SECS=60
```

Recommended rollout: `APPS_CONTROLLER=true` + `APPS_DRIVE_MODE=dry-run`
first (watch `DriveIntended` Events + logs across the fleet), then flip
`APPS_DRIVE_MODE=on` with a single-app `APPS_DRIVE_ALLOW`, widen one app
at a time, end at `APPS_DRIVE_ALLOW=*` only once trusted.

### RBAC + Events

No new RBAC: the existing operator ClusterRole already grants
`apps/deployments` get/list/watch/patch and core `events` create/patch.
The controller emits namespaced Events (`reason` ∈ {`DriveIntended`,
`Driven`, `DriveRolloutPending`, `DriveFailed`}) against a synthetic
`involvedObject` kind `App` named by the lifecycle id — readable via
`kubectl get events` and surfaceable on `platform.hanzo.ai/apps`. Event
write failures are non-fatal (logged, swallowed). A failed sweep
(platform unreachable/auth) logs and retries next tick — it never crashes
the operator.

Test count: 94 → 103 lib tests (+9 apps controller gate/rollout, plus the
apps_client semver/image-ref/wire-shape suite; 0 regressions).

## v0.6.11 — tenant-RBAC controller (per-tenant one-click deploy)

`src/controllers/tenant_rbac.rs`. Not a CRD Kind — its reconcile source is the
set of platform-managed tenant namespaces (`tenant-<org>`, labeled
`hanzo.ai/managed-by=platform`, created by cloud-api on org onboarding). For each
it server-side-applies a NAMESPACED RoleBinding `cloud-api-platform` binding
ClusterRole `hanzo-cloud-platform-tenant` to ServiceAccount `hanzo/cloud-api`,
so the cloud-api SA can `/v1/platform` deploy Hanzo `Service` CRs INTO that one
tenant namespace — and nowhere else.

CRITICAL (RED): it is a RoleBinding, NEVER a ClusterRoleBinding. A
ClusterRoleBinding of the tenant role would let cloud-api deploy into EVERY
namespace (a cross-tenant deploy hole). The namespaced RoleBinding confines the
grant; cloud-api can write only to namespaces that have been onboarded, and each
grant is independently revocable. Verified live: `can-i create services.hanzo.ai`
as `hanzo/cloud-api` = yes in tenant-<org>, NO in `default`/`kube-system`; the SA
cannot create ClusterRoleBindings at all.

Gate: master enable `TENANT_RBAC_CONTROLLER` (default `true`, opt-out). Config
overrides for white-label: `TENANT_RBAC_LABEL`, `TENANT_RBAC_SA_NAMESPACE`,
`TENANT_RBAC_SA_NAME`, `TENANT_RBAC_CLUSTER_ROLE` (Hanzo defaults baked in).

New RBAC the operator ClusterRole needs (declared in
`hanzoai/universe infra/k8s/operator/deployment.yaml`): `namespaces`
get/list/watch, `rolebindings` CRUD, and `bind` on ClusterRole
`hanzo-cloud-platform-tenant` (RBAC escalation guard — narrowly scoped so the
operator can never be leveraged to bind a broader role). The two cloud-api
platform ClusterRoles + the static build-ns RoleBinding are declared in
`infra/k8s/operator/rbac/cloud-platform-rbac.yaml`.

Deploy note: the operator is a leader-elected SINGLE replica whose readiness is
gated on holding the lease. A default rollingUpdate deadlocks (maxUnavailable
rounds to 0 for 1 replica; new pod can't become leader while old renews the
lease). The Deployment now uses `strategy: Recreate`.

Test count: 103 → 108 lib tests (+5 tenant_rbac: namespaced-not-cluster-wide,
roleRef+subject shape, org label/prefix extraction, white-label env overrides,
label-key split; total suite 131; 0 regressions).

## v0.6.12 — tenant-RBAC controller also provisions the per-tenant ghcr-pull Secret (RED HIGH-2)

Alongside the `cloud-api-platform` RoleBinding, `tenant_rbac.rs` now projects the
`ghcr-pull` image-pull Secret into each tenant namespace so pods can pull the
PRIVATE per-tenant build image (`ghcr.io/<org>/tenant-<org>/*`). This closes RED
HIGH-2: cloud-api must NOT touch K8s Secrets (KMS-only model — devs/services never
touch secrets, the operator does). The prior code had cloud-api's `ensurePullSecret`
creating the Secret (wrong SA); that is DELETED from cloud and moved HERE.

- `build_pull_secret` (pure): a `kubernetes.io/dockerconfigjson` Secret with a
  DISTINCT `managed-by` (`hanzo-operator-tenant-rbac`) so the `core::secret`
  hijack guard never cross-adopts the RoleBinding or KMS-zap Secrets.
  `pull_config_bytes` reads `data` (base64-decoded) then `string_data`.
- `ensure_pull_secret` (async): read the KMS-synced SOURCE (`hanzo/ghcr-secret`,
  `.dockerconfigjson`) → `validate_secret_value` → hijack-guard the destination
  (`is_operator_managed`, refuse overwriting an unmanaged same-named Secret) → SSA
  `apply`. FAIL-OPEN: a missing source / error is logged + retried; the deploy
  RoleBinding always applies, so deploy authz is never blocked by pull-secret trouble.
- Config gains `pull_secret_name`/`pull_source_namespace`/`pull_source_name`/
  `pull_config_key` (env `TENANT_RBAC_PULL_*`). Defaults `ghcr-pull` / `hanzo` /
  `ghcr-secret` / `.dockerconfigjson`.

New RBAC (universe `infra/k8s/operator/deployment.yaml`): operator-manager-role
gains `secrets [get, create, patch]` (least privilege — no delete/list/watch).
cloud-api's two platform ClusterRoles are UNCHANGED and hold NO `secrets` verb.

Test count: 131 → 136 lib tests (+5 tenant_rbac pull-secret; 0 regressions).

## v0.6.13 — zero-downtime rolling handoff for single-writer PVC services

`manifests::build_deployment` now injects a **soft (preferred) self-podAffinity**
(`weight 100`, `topologyKey kubernetes.io/hostname`, `labelSelector = the app's
own selector labels`) whenever a Deployment BOTH uses `strategy != Recreate`
(i.e. RollingUpdate, which the operator already renders as
`maxSurge:1 / maxUnavailable:0`) AND mounts a `persistentVolumeClaim` volume.

Why: a RollingUpdate over a **ReadWriteOnce** PVC (DO block storage is
single-attach) deadlocks if the surge pod lands on a different node than the
volume's current holder — a "Multi-Attach" error. Co-locating the surge pod on
the SAME node as the running pod (RWO permits multiple pods per NODE) makes the
new pod bind-mount the already-attached volume with **no detach/reattach gap**,
so the roll is genuinely zero-downtime. It is also the ONLY node topology under
which a per-tenant SQLite writer stays safe during the brief two-pod overlap:
WAL's `-shm` index is an mmap shared only within one host, and POSIX file locks +
`busy_timeout` serialize the overlap → no corruption, no cross-node split brain.

The affinity is **soft, never required**: with no anchor pod (cold start / node
loss) the surge schedules anywhere and recovers; a rare failure to co-locate
degrades to a fail-SAFE stalled roll (old pod keeps serving under
`maxUnavailable:0`), never an outage. `Recreate` services and volume-less
services are untouched (affinity stays `None`) — so this is a **no-op for the
entire fleet until a CR opts in with `strategy: RollingUpdate` on a PVC-backed
service** (first consumer: `cloud`, the api.cloud.hanzo.ai backend, whose unified
binary opens every per-tenant store WAL + `busy_timeout` and runs idempotent
migrations, making the same-host overlap safe).

Test count: 136 → 139 lib tests (+3 `manifests::deployment_tests`:
rolling+PVC co-locates soft/self/hostname, Recreate+PVC has no affinity,
rolling+no-PVC has no affinity; 0 regressions).
