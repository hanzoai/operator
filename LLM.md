# Hanzo Operator — AI-Friendly Guide

## What
Canonical Kubernetes operator for the Hanzo platform. Rust implementation,
shared by Hanzo, Lux, Zoo, and Osage universes.

One binary. 28 CRD Kinds. No compat aliases — the v1 Kinds are the one
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

## CRD Kinds (28 total)

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

### Autonomous-bot (v1)
| Kind            | Short | Converges (HTTP, not in-cluster objects) |
|-----------------|-------|------------------------------------------|
| AgentDeployment | bot   | cloud `/v1/agents` Agent + visor `/v1/machines` bound `@hanzo/bot` machine |

## Layout
```
src/
  main.rs           Entrypoint — clap args, leader election, controller spawn.
  lib.rs            Library facade.
  crd.rs            All 28 CRD types.
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

## AgentDeployment — the autonomous-bot lifecycle Kind

`AgentDeployment` (`agentdeployments.hanzo.ai`, short `bot`/`agentdeploy`) is
the 28th Kind: the declarative desired state of a **Bot** = Agent
(`execution_mode=long-running`) + a visor-provisioned machine running the
`@hanzo/bot` runtime. Spec: `{agentName, org, executionMode(=long-running),
schedule?, replicas?, botVersion?, provider?, machineId?}`.

Unlike the other CRD controllers (which materialize in-cluster K8s objects),
its reconcile ACTIONS reach TWO external control planes over HTTP — it
composes the `managed_database` watch pattern with the `apps` HTTP-client
pattern rather than inventing a third:

1. **cloud `/v1/agents`** (`core::agents_client`) — ensure the Agent exists
   with the desired execution mode (get-then-create).
2. **visor `/v1/machines`** (`core::visor_client`) — bind (or launch+bind) a
   machine to the `@hanzo/bot` runtime via `POST /v1/machines/:id/bind-agent`.

`status.phase` mirrors the honest visor binding status (`Pending`/`Bound`/
`Error`) — `Running` only when the Agent is ready AND the binding is `Bound`.

### Safety — provisioning is opt-in + fail-safe (mirrors the apps controller)

This controller can create cloud Agents and LAUNCH cloud machines (which cost
money), so mutation is gated:

- **`AGENT_DEPLOY_MODE`** ∈ {`off` (default), `bind-only`, `on`}:
  - `off` — read-only: report status, never create/launch/bind.
  - `bind-only` — may create the Agent + bind an EXISTING `spec.machineId`,
    but NEVER launches (zero-cost).
  - `on` — may additionally launch a machine when `spec.provider` is set and
    no `spec.machineId` is given.
- Without `AGENT_DEPLOY_CLOUD_URL` / `AGENT_DEPLOY_VISOR_URL` + a service
  token (`AGENT_DEPLOY_SERVICE_TOKEN` | `PLATFORM_SERVICE_TOKEN` |
  `HANZO_API_KEY`), the controller runs READ-ONLY regardless of mode.

Visor auth: visor authorizes the operator as the `app` subject via its IAM
application **clientId/clientSecret** presented as HTTP **Basic** auth — it does
NOT parse `Authorization: Bearer`. Set `AGENT_DEPLOY_VISOR_CLIENT_ID` /
`AGENT_DEPLOY_VISOR_CLIENT_SECRET` (fallback `IAM_CLIENT_ID` /
`IAM_CLIENT_SECRET`); without them visor denies the path-scoped
`/v1/machines/:id/...` binding routes (403) and provisioning silently no-ops.
`visor_client` sends exactly ONE `Authorization` header (Basic when creds are
set, else Bearer) — reqwest appends, and two headers would break visor's
`Request.BasicAuth()`. Cloud `/v1/agents` still uses Bearer (it is bearer-aware).

`ProvisionPlan::for_spec(mode, spec) -> ReadOnly | BindExisting |
LaunchThenBind | NoTarget` is pure and the unit of test; `machineId` always
wins over `provider` (cheapest safe path), and `bind-only` refuses to launch
even with a `provider`.

Files: `src/crd.rs` (AgentDeploymentSpec/Status), `src/core/agents_client.rs`,
`src/core/visor_client.rs`, `src/controllers/agent_deployment.rs`. Wired into
`main.rs` `tokio::join!` + the `generate-crd-yaml` bundle + the four
`k8s/crds/all-*.yaml` bundles.

Test count: 110 → 136 lib tests (+9 agent_deployment gate/condition, +9
agents_client envelope/mode incl. `"data"`-substring regression, +8
visor_client envelope/spec/auth-header; bundle test 27→28; 0 regressions).
`cargo build`/`clippy -D warnings`/`fmt --check`/`test` all clean.

## v0.6.17 — surge co-location OPT-IN, forward-ported onto main (zero-downtime SQLite-WAL deploys)

The 0.6.13/0.6.14 surge co-location feature was authored on branch
`fix/cloud-zero-downtime-rwo-colocation` but **never merged to main** — the
0.6.15/0.6.16 tags were cut from a main that lacks it, so the live
`0.6.16-amd64` binary silently ignores `spec.surgeColocation` even though the
CRD (universe `crds.yaml`, ahead of the binary) carries the field. Confirmed
live: patching `surgeColocation: true` on the iam CR flipped strategy to
RollingUpdate but injected NO affinity (0.6.16 has no `should_colocate`).

v0.6.17 forward-ports ONLY the additive surge pieces onto current main (which
already renders RollingUpdate as maxSurge=1/maxUnavailable=0 and has the newer
Kinds — AgentDeployment/ManagedDatabase/probe-handlers — that the old branch
predates, so the whole-file merge was NOT usable):

- `crd.rs`: `ServiceSpec.surge_colocation: bool` (default false, camelCase
  `surgeColocation`).
- `manifests.rs`: the pure `colocation_affinity(selector)` — soft (preferred,
  weight 100) self-podAffinity on `kubernetes.io/hostname`.
- `controllers/service.rs`: `should_colocate(surge, strategy, mounts_pvc) =
  surge && strategy != "Recreate" && mounts_pvc`, and the injection in
  `reconcile_service_inner` (post-build, iff the gate opens) using
  `sel_labels`. `mounts_pvc` is computed from the resolved `volumes_k8s`.

Semantics (unchanged from 0.6.14): a RollingUpdate service that opts in AND
mounts a PVC gets a surge pod softly pinned to the volume's node, so it
bind-mounts the already-attached RWO volume (no Multi-Attach deadlock) — a
zero-downtime same-node handoff. SAFE ONLY for a store that tolerates a brief
same-host two-pod overlap: SQLite WAL + `busy_timeout` (+ per-file flock for
DEK-mint). Exclusive-lock engines (cloud's Badger KMS + in-memory audit seq)
MUST stay `strategy: Recreate` + `surgeColocation: false` — verified UNSAFE
live under the 0.6.x experiment. First real consumers: `iam` and `commerce`
(both per-org SQLCipher-WAL via `github.com/hanzoai/sqlite`, no exclusive-lock
engine). No-op for the entire fleet until a CR opts in.

Test count: 145 → 147 lib tests (+2 service: `should_colocate` gate +
`colocation_affinity` soft/self/hostname shape; 0 regressions). My files
compile + fmt-clean; the 2 pre-existing clippy doc-indent warnings in
`datastore.rs` are untouched (out of scope).

## v0.6.22 — Tenant onboarding controller (per-tenant one-click deploy)

The sole go-live blocker for Hanzo PaaS: cloud's deploy path
(`clients/platform/k8s.go` `waitForTenantRBAC`) BLOCKS on a
SelfSubjectAccessReview poll until the operator has onboarded a
freshly-created `tenant-<org>` namespace. `src/controllers/tenant.rs`
(reconciled from branches `batcha/tenant-rolebinding` + `feat/tenant-pull-secret`)
watches namespaces filtered to `hanzo.ai/managed-by=platform` and, for each,
SSA-applies the two objects cloud waits on:

1. a **namespaced** `RoleBinding` `cloud-api-platform` → ClusterRole
   `hanzo-cloud-platform-tenant`, subject `hanzo/cloud-api` ServiceAccount —
   NEVER a ClusterRoleBinding (that would be a cross-tenant deploy hole); the
   grant is confined to the one tenant namespace and independently revocable.
2. a `ghcr-pull` `kubernetes.io/dockerconfigjson` image-pull `Secret`, projected
   from the KMS-synced source `hanzo/ghcr-secret`, so tenant pods can pull the
   PRIVATE per-tenant build image `ghcr.io/hanzoai/tenant-<org>/*`. cloud-api
   holds NO `secrets` grant — the operator is the designated K8s-secret handler.

Both children carry the `hanzo.ai/managed-by=platform` label and a Namespace
owner reference so they GC with the tenant. The Secret path is fail-OPEN (a
missing/empty source is logged, the RoleBinding still lands so deploy AUTHZ is
never blocked) and hijack-guarded (`managed-by=hanzo-operator-tenant-rbac` —
never overwrites a Secret it does not own). `Config::from_env()` white-labels
every name (SA, ClusterRole, source/target Secret) for the lux/zoo/osage
universes. Gate: `TENANT_CONTROLLER` (default on).

Contract completeness lives in `hanzoai/universe`
(`infra/k8s/operator/deployment.yaml` + `rbac/cloud-platform-rbac.yaml`, both
already declared): the `hanzo-cloud-platform-tenant` ClusterRole (services.hanzo.ai
+ resourcequotas/limitranges + datastores/docdbs/manageddatabases + secrets +
pvc + kmssecrets), and the operator's own ClusterRole grants (`namespaces`
get/list/watch, `rolebindings` CRUD, `clusterroles` `bind` scoped via
`resourceNames: [hanzo-cloud-platform-tenant]`).

Test count: 168 → 179 lib tests (+11 tenant: namespaced/confined binding,
roleRef+subject, platform-label+owner-ref, org resolution, white-label env,
dockerconfigjson shape, hijack-manager isolation, source-bytes precedence;
0 regressions). fmt-clean; no new clippy warnings (my file is clean; the 4
pre-existing warnings in datastore.rs/manifests.rs are out of scope).

## v0.7.0 — the three verbs: INSTALL, OPERATE, UPGRADE (deploy discipline in-operator)

The operator now INSTALLS, OPERATES, and UPGRADES the platform — no human
hand-patches a CR, hand-watches a rollout, or hand-writes an auto-rollback loop.
The deploy discipline is encoded as a reconcile state machine.

### UPGRADE — managed-upgrade FSM (the core)

When a `Service` opts into `spec.upgradePolicy` (AND the operator's cluster gate
`UPGRADE_FSM_ENABLED` is on), a change to `spec.image` rolls through a pure state
machine instead of a blind apply. Files:
- `src/controllers/upgrade.rs` — the pure decision core `plan(Observed, Cfg) ->
  Plan` (the brain, unit-tested with values, no cluster) + the pre-flight
  resource builders/converge/observe (the thin imperative shell).
- `src/core/health.rs` — pure health predicates (`rollout_complete` — LIFTED here
  from `apps.rs` so both call sites share ONE definition; `deployment_healthy`,
  `pod_boot_outcome`, `pod_crashlooping`).
- `src/controllers/service.rs` — `drive_upgrade_fsm` gathers live observations,
  runs `plan`, and threads the decision into the Deployment image + status.

**States** (`status.upgrade.phase`; absent ⇒ Stable): `Preflighting` → `Rolling`
→ (Stable | `RollingBack` → `Failed`).

**The gates, in order:**
1. **Pre-flight** the candidate BEFORE flipping. For a stateful Service, boot it
   against a CSI-`VolumeSnapshot` CLONE of the live data PVC (mounted at the
   persistence `dataDir`), with the app's REAL master key (`envFrom`) + a
   boot-only env overlay (`spec.upgradePolicy.bootEnv`, e.g. `CLOUD_ENV=smoke`),
   `restartPolicy: Never`, no Service. This is the operator-side analog of
   cloud's CI migration-smoke (`internal/migratetest` + release.yml two-boot),
   but STRONGER — it tests the candidate over the ACTUAL current schema, not a
   pinned baseline. A candidate that cannot boot over real data (an
   index-before-ADD-COLUMN migration crash) FAILS the pre-flight and production
   is NEVER flipped. Stateless Services pre-flight a boot-only pod (no clone).
2. **Health-gate** the rollout: flip the Deployment image, watch readiness within
   `rolloutDeadlineSeconds` (default 300).
3. **Auto-rollback**: if the candidate crash-loops (fast path via
   `pod_crashlooping`) or misses readiness by the deadline, revert the Deployment
   to `status.lastGoodImage` and record the failure.

**THE INVARIANT** (`never_leaves_prod_on_an_unproven_or_failed_candidate`):
`effective_image == lastGood` in every state where the candidate is unproven or
failed; it is the candidate ONLY in Rolling (after the pre-flight passed);
`lastGood` advances to the candidate ONLY on proven success. A failed target
lands in terminal `Failed`, keyed to the image — the operator holds prod on
`lastGood` and NEVER re-flips it; only a NEW `spec.image` reopens the FSM (a
supersede).

**Resumable + split-brain-safe**: every input is read from `status.upgrade` +
the live cluster, so `plan` re-derives the same action after an operator restart
(the pre-flight pod is named deterministically by target hash). The Service
controller runs only after the lease is held (`main.rs` blocks on `leader_flag`),
so single-writer election is the fail-closed split-brain guard — a non-leader
never reconciles.

**Gate (fail-safe drop-in)**: OFF by default. Cluster kill-switch
`UPGRADE_FSM_ENABLED=true` AND per-CR `spec.upgradePolicy.enabled=true` are BOTH
required; otherwise the operator applies `spec.image` directly (historical
behavior, zero change). First deploy of this binary is inert.

**New CRD fields** (additive, backward-compatible): `spec.upgradePolicy`
{`enabled`, `preflight`, `rolloutDeadlineSeconds`, `preflightDeadlineSeconds`,
`bootEnv[]`, `snapshotClass`}; `status.lastGoodImage`, `status.upgrade`,
`status.upgradeHistory[]`. Regenerate the `k8s/crds/all-*.yaml` bundles after any
change (`generate-crd-yaml --api-group <g>`).

### OPERATE

The existing `Service` reconcile (Deployment/Service/Ingress/HPA/PDB/NP/KMSSecret)
is unchanged; the FSM composes with it by overriding only the Deployment image
(`reconcile_service_inner(..., effective_image)`).

### INSTALL — `operator install` / `operator up` (the bootstrap seam)

`src/install.rs` + `main.rs` subcommands (`hanzod install` execs into
`operator install`):
- `operator install [--image X] [--upgrade-fsm] [--crds-only]` — SSA-apply the
  derived CRDs (from the `CustomResource` derives via `crd_bundle`, the ONE home
  now shared with `generate-crd-yaml`) + the operator's own namespace /
  ServiceAccount / ClusterRole / ClusterRoleBinding / Deployment. Idempotent.
- `operator up [--image X] [--manifests DIR] [--upgrade-fsm]` — install, then
  apply the platform's own App CRs from `DIR` (kind-agnostic dynamic apply via
  discovery) so the running operator brings the stack up. `k3s`-bootstrap when no
  cluster is a documented phased seam.

The rendered operator ClusterRole includes the NEW pre-flight grants: core
`pods` + `persistentvolumeclaims` (create/delete for the clone), and
`snapshot.storage.k8s.io/volumesnapshots` (the CSI clone source). **The live
operator ClusterRole in `hanzoai/universe` MUST be extended with these three
before enabling the FSM in-cluster** (deploy-gate).

### Shadow-proven vs live cutover

SHADOW/UNIT-PROVEN (no live prod touched): the pure FSM across the full lifecycle
(happy upgrade; migration-crash pre-flight never reaches prod; crashloop →
auto-rollback → prod stays on lastGood; resume-after-restart), the pre-flight
builders (clone-not-live-PVC, `restartPolicy: Never`, boot-env), health
predicates, install manifests, and the CRD schema carrying the new fields.
LIVE-CUTOVER-GATED (needs a real CSI driver with a VolumeSnapshotClass + the
universe RBAC delta): the end-to-end CSI snapshot→clone→boot round-trip. The
legacy Go operator stays in production; hanzod is SHADOW.

Test count: 179 → 240 lib tests (+15 core::health, +37 controllers::upgrade incl.
the full-lifecycle sequence tests + the invariant sweep + builder shapes, +6
install; -4 apps rollout tests moved to core::health; 0 regressions). My files
are fmt-clean + clippy-clean; the pre-existing datastore/manifests/ingress/tenant
fmt+clippy drift on origin/main is untouched.
