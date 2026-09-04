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

## Canonical role & brand (SDK model)

**Canonical impl repo.** This is infrastructure, not an SDK — it lives at its
canonical home `hanzoai/operator` (Rust, web2 canonical) with its Go sibling
`luxfi/operator` (web3 canonical). One impl, one place: discovery/marketing
repos link here, they never re-document the reconcile logic. Full model in
`~/work/hanzo/SDK-ARCHITECTURE.md`.

**Brand rules — hard, enforce in every doc/string written here:**
- Hanzo is the **Open AI Cloud** — a full AI SDK / AI cloud, never an "LLM
  gateway" and never positioned against LiteLLM or as an "OpenAI-compatible
  proxy". (The `LLM` CRD Kind is just a `Service` facade for AI workloads —
  that is a Kind name, not that framing.)
- Paths are `/v1/...` only — never an `/api/` prefix.
- Zen models are our own family; never name upstream models.

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
| S3      | s3    | Datastore (engine=s3, SeaweedFS) |
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
  canonical value (`postgresql` / `valkey` / `docdb` / `s3`) before
  delegating to `datastore::reconcile_datastore_inner_pub`. This makes
  the facade kind authoritative — a `SQL` CR cannot accidentally
  materialize as a Valkey or S3 datastore even if the user sets
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
   starts; mirrors `KMS_PROJECTOR`). First deploy of this binary is
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
(the pre-flight pod is named deterministically by target hash). Controllers run
only while the lease is held — `main.rs` blocks on `leader_flag` to start them
and hosts them in the `select!` the election returns from, so losing the lease
drops them (see v0.7.7). Single-writer election is the fail-closed split-brain
guard — a non-leader never reconciles.

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

## v0.7.2 — native git → CR reconcile (folds the gitops-reconcile CronJob INTO the operator)

The git→cluster apply step is now a NATIVE loop inside the operator, so the
operator does the whole chain — **git → CR → workload** — in one process. It
replaces the external `gitops-reconcile` CronJob
(`hanzoai/universe/infra/k8s/gitops-reconcile`): a 5-min `alpine/k8s`
`kubectl apply` stopgap that could not reconcile its own spec and carried a
hand-maintained 24-item allow-list in its env.

New component: `src/controllers/gitops.rs` (one cohesive file) + a small
`apply::apply_dynamic_as(field_manager)` helper, wired into `main.rs`'s
`tokio::join!` alongside every other controller (so it runs only on the elected
leader) and gated OFF by default.

- **Source** — `infra/k8s/operator/crs/*.yaml` on `hanzoai/universe` main, read
  over the GitHub REST API with `reqwest` (the operator image ships no `git`):
  Contents API lists the dir, git-blobs API fetches content. The token is the
  same KMS-synced secret the CronJob used (`gitops-repo-creds`, mounted at
  `/creds/token`; `GITOPS_TOKEN_FILE`), re-read each sweep so rotation needs no
  restart. It rides an `Authorization` header — never a URL — and is wrapped in a
  redacting `Token` newtype, so it can never land in a log line. Blobs are
  content-addressed by SHA and cached, so a tight poll only refetches files that
  actually changed (rate-limit-safe). `GITOPS_GITHUB_API` lets a
  GitHub-compatible host (git.hanzo.ai) serve the same code later.
- **Apply** — server-side apply (force-conflicts) under the distinct field
  manager `hanzo-operator-gitops`, so a git-driven apply is attributable and
  never fights a per-Kind reconcile. **Kind-agnostic:** each file's own
  `apiVersion`/`kind` is applied, and the correct plural (e.g.
  `hanzo.ai/v1 Ingress` → `ingresses`, not a naive `ingresss`) is resolved via
  `kube::discovery::pinned_kind` — the same mechanism `kubectl` uses — cached per
  Kind. Handles today's `Service`/`SQL`/`LLM`/`KV`/`DNS`/`Ingress` +
  `secrets.lux.network KMSSecret` + core `PersistentVolumeClaim`, and the
  forthcoming `App` (App-collapse) with ZERO code change.
- **NEVER prune** — a CR removed from git is left alone, identical to
  `reconcile.sh`. This is a property of the type, not a runtime check: the pure
  `Plan` can only describe applies (no delete variant exists anywhere in the
  module), so a removed file produces no plan entry at all. Locked by the
  `plan_never_prunes_a_removed_file` test.
- **Drift report** — every sweep GETs each object before applying and logs a
  per-object `CREATE` / `UPDATE (reverted drift)` / in-sync line plus a summary.

### Cadence — real-time, not 5-min cron

A tight resync loop (default 45s, `GITOPS_RESYNC_SECS`, floored at 10s) is the
baseline. `POST /reconcile` on the operator's existing health server triggers an
INSTANT sweep — a git.hanzo.ai/GitHub push webhook can drive it — via a shared
`tokio::sync::Notify`; each loop iteration wakes on whichever fires first (poll
tick or webhook), so the poll is the guaranteed fallback and the webhook is the
real-time path.

### Scope — ownership, not a hand-maintained allow-list

The DEFAULT scope is ownership-by-namespace: every platform CR in the operator's
namespace (`hanzo`, `GITOPS_NAMESPACE`). All 78 crs/ CRs declare
`namespace: hanzo`, so the namespace predicate cleanly IS the platform boundary
(a CR in any other namespace, or cluster-scoped, is out of scope). The safety
config `GITOPS_APPLY_SCOPE` (comma/space-separated CR names) optionally NARROWS
to a vetted subset for a cautious first rollout — clear it (widen to the whole
namespace) once trusted. This replaces the CronJob's `RECONCILE_ALLOWLIST`; the
model is now ownership, the list is just an optional throttle.

### Fail-safe

Opt-in `GITOPS_RECONCILE_ENABLED=true` (default off — first deploy of this binary
is inert; mirrors `KMS_PROJECTOR`/`APPS_CONTROLLER`). Additive: it runs
ALONGSIDE the CR→workload controllers and never blocks them. Every fallible op
inside a sweep returns `Result` and is logged; the loop never `?`-propagates out
of its body, never `unwrap`s, never panics — a clone/list/YAML/token failure is
logged and retried next tick.

### Enabling it (operator Deployment env in `hanzoai/universe`)

```
GITOPS_RECONCILE_ENABLED=true        # master enable (default off)
# defaults are correct for prod; override only to change source/scope:
# GITOPS_REPO=hanzoai/universe  GITOPS_BRANCH=main
# GITOPS_CRS_PATH=infra/k8s/operator/crs  GITOPS_NAMESPACE=hanzo
# GITOPS_TOKEN_FILE=/creds/token  GITOPS_RESYNC_SECS=45
# GITOPS_APPLY_SCOPE="functions admin-guard ..."  # optional vetted subset first, then clear
```
Mount the existing `gitops-repo-creds` secret at `/creds` (the KMS sync already
provisions it). **RBAC:** the operator's ClusterRole must grant `create`/`update`/
`patch` (NO `delete`) on the CR groups it now writes — `hanzo.ai/*`,
`secrets.lux.network/kmssecrets`, and core `persistentvolumeclaims` — i.e. the
verbs the `gitops-reconcile` ServiceAccount held
(`infra/k8s/operator/gitops-reconcile/rbac.yaml`). Fold those rules into the
operator ClusterRole before enabling.

### Cutover — DELETE the CronJob after 0.6.24 deploys (gated; NOT done here)

Once `ghcr.io/hanzoai/operator:0.6.24` is live with
`GITOPS_RECONCILE_ENABLED=true` and a sweep has logged "reconcile sweep
complete", the external loop is redundant. Delete it (a `hanzoai/universe`
manifest change + a prod action):

```
# in hanzoai/universe: remove infra/k8s/operator/gitops-reconcile from the
# kustomization, then reap the objects it created:
kubectl -n hanzo delete configmap gitops-reconcile gitops-reconcile-script
kubectl -n hanzo delete role,rolebinding gitops-reconcile-canary
kubectl delete clusterrole,clusterrolebinding gitops-reconcile
kubectl -n hanzo delete serviceaccount gitops-reconcile
# KEEP gitops-repo-creds + gitops-repo-creds-kms-sync — the operator mounts the
# same token (one credential, one KMS sync).
```

Version note: this loop was cut from the v0.6.24 base and rebased forward onto
v0.7.1 (v0.7.0's upgrade FSM + the App Kind), shipping as **v0.7.2**. It
re-applies cleanly — `gitops.rs` is a new file, `apply::apply_dynamic_as` is a
pure addition, and the `main.rs` wiring (the `HealthState` / `POST /reconcile`
webhook + the `run_gitops_controller` join arm) composes with v0.7.0's
`install`/`up` subcommands and the App controller without touching either.
**INERT by default**: with `GITOPS_RECONCILE_ENABLED` unset the loop never starts,
so the first 0.7.2 deploy is a no-op for the git path — the external
`gitops-reconcile` CronJob stays the git→etcd path until the flag is flipped
post-App-cutover.

Test count: 253 → 268 lib tests (+15 gitops: file selection, kind-agnostic GVK
parse, scope predicate incl. namespace boundary + vetted subset, never-prune
invariant, drift skips, base64 blob decode, resync floor, token redaction; the
App Kind's 13 tests and every prior suite intact; 0 regressions). `cargo build
--release` + `cargo test --lib` green (268 passed / 0 failed); `gitops.rs` +
`main.rs` fmt-clean + clippy-clean. Clippy repo-wide is blocked by the global
`~/.cargo/config.toml` `rustc-wrapper=zccache` (feeds rustc as an input filename
to clippy-driver); neutralizing the wrapper (`RUSTC_WRAPPER=""`) for one run
confirms my files are warning-free. The pre-existing datastore/manifests
fmt+clippy drift on origin/main is untouched.

## Pre-flight hardening — labels, egress additivity, orphan GC (pre-enable blockers)

Red re-review of the pre-flight surfaced blockers that only matter once
`UPGRADE_FSM_ENABLED` is flipped ON (the shadow/off binary was already safe).
All fixed; the gate stays OFF until the deploy-gates below are met on a live
cluster.

**Pre-flight labels are a MINIMAL functional set — never a shared selector key**
(HIGH-1 + HIGH-2). A ClusterIP Service selects pods by EXACTLY
`{app.kubernetes.io/name, app.kubernetes.io/instance}`; a pre-flight pod that
carried those would be added by the EndpointSlice controller as a LIVE endpoint
of the production Service (kube-proxy load-balances real user traffic onto the
unproven candidate — stale clone reads, silently-discarded writes), and an
app-labelled egress-allow NetworkPolicy would re-select it (egress is additive).
The FIRST fix stripped `name`/`instance`. But the SHARED descriptive keys
(`component`/`part-of`/`version`) are the SAME re-open vector via a
descriptive-label selector — a headless discovery/metrics Service with
`selector:{app.kubernetes.io/part-of: hanzo}` (→ EndpointSlice adds the candidate
as a live endpoint, HIGH-1) or a baseline egress-allow NetworkPolicy
`podSelector:{app.kubernetes.io/part-of: hanzo}` ("all hanzo pods may reach the
DB" → egress union re-grants the candidate real DB egress, HIGH-2). Those keys
serve ZERO function on a throwaway pre-flight resource (netpol/sweep/GC key ONLY
on `preflight-of`/`preflight-target`), so they are pure selector surface and are
dropped — MINIMIZE AT THE SOURCE, don't widen the enable checklist.

Fix (double belt, matching the name/instance pattern): (1) `build_preflight_inputs`
seeds the pre-flight `labels` from the new `manifests::managed_by_labels()` (JUST
`app.kubernetes.io/managed-by` — attribution, non-selecting) rather than
`descriptive_labels`; (2) `upgrade::pf_labels` ALSO strips `name`/`instance`
**and** `component`/`part-of`/`version` unconditionally, so the invariant holds
for ANY base. The dedicated set is EXACTLY three keys:
`{hanzo.ai/preflight-of, hanzo.ai/preflight-target, app.kubernetes.io/managed-by}`.
`descriptive_labels = managed_by_labels() ∪ {component,part-of,version}` and
`standard_labels = selector_labels ∪ descriptive_labels` (DRY — one home for the
`managed-by → hanzo-operator` mapping). Test:
`preflight_labels_exclude_the_service_selector_keys` — all four resource kinds
(pod/netpol/clone-PVC/snapshot) drop name/instance AND component/part-of/version,
carry EXACTLY the three functional keys (`labels.len() == 3`), and the production
Service selector is not a subset of the pre-flight labels (synthetic-endpoint
check). The adversarial base is `standard_labels` (carries all five stripped
keys) so the strip is genuinely exercised, not vacuous.

**Egress additivity is stated honestly, and the empty-selector residual is a
deploy-gate** (HIGH-2). NetworkPolicy egress is a UNION across every policy that
selects a pod; an empty egress rule set grants NO egress but cannot OVERRIDE an
`allow` in another policy. With the pre-flight pod carrying dedicated labels, an
app-labelled egress-allow can no longer re-select it — but a namespace-wide
`podSelector: {}` egress-allow (e.g. an allow-DNS-to-all baseline) still does,
and labels cannot escape that. So `build_preflight_netpol` now documents the
additivity PRECONDITION instead of asserting a guarantee, and the test is named
`…denies_all_egress…` no longer implies an unconditional guarantee (the
comment/precondition is explicit). DEPLOY-GATE (pre-enable, per target
namespace): **no `podSelector: {}` egress-allow policy selects the pre-flight
pod.** A DEDICATED strict-default-deny namespace for the pre-flight was assessed
and rejected as infeasible in the operator model: the VolumeSnapshot, its source
PVC, and the clone-from-snapshot PVC are all namespace-local and the candidate
pod must mount the clone in that namespace — CSI has no clean cross-namespace
snapshot/restore — so the pre-flight MUST run in the app's namespace. The
namespace-egress-allow check is therefore the enable-gate.

**Orphan GC — startup + periodic** (MED-1). `converge_preflight` sweeps a
Service's own pre-flight each reconcile, but a disable-sweep that silently failed
(`delete_stale_preflight` swallows errors, then `upgrade_inactive` clears
`status.upgrade` so no later reconcile retries), or an operator crash
mid-pre-flight before status was written, leaks a clone PVC + VolumeSnapshot
(FULL copies of live tenant data) + the real-credential candidate pod with no
retry. `controllers::service::run_preflight_gc` (wired into `main.rs`, leader-
gated, INDEPENDENT of `UPGRADE_FSM_ENABLED` so a gate-OFF-after-leak still
reclaims) lists every `hanzo.ai/preflight-of` resource cluster-wide
(`upgrade::list_preflight`) and reclaims each whose owning Service has no
matching in-flight pre-flight. The orphan decision is PURE + tested
(`liveness_from_status` → `is_preflight_orphan`): a Service is "in-flight" iff
`status.upgrade.phase == Preflighting` with a matching target hash; anything else
(done/failed/Rolling/gone) ⇒ orphan; a Service whose status can't be read ⇒
`Unknown` ⇒ never reclaimed (fail-closed). A `PREFLIGHT_GC_GRACE_SECS` (default
300) window skips freshly-created resources so the periodic sweep never races a
reconcile mid-create. Env: `PREFLIGHT_GC_INTERVAL_SECS` (default 600, floor 30),
`PREFLIGHT_GC_GRACE_SECS` (default 300). No new RBAC (list/delete on
pods/pvc/networkpolicies/volumesnapshots already granted).

**Pre-flight pod does not automount the SA token** (LOW-1):
`automountServiceAccountToken: false` on the pre-flight PodSpec — a boot-to-ready
check needs no k8s API access, and an unmounted token is unusable if egress ever
leaks. Fail-closed (an app that needs the token to boot fails the pre-flight).

**Status writeback carries a resourceVersion precondition** (LOW-2): the Service
status patch pins the observed `resourceVersion`, so a stale ex-leader writing in
the ~10s lease-overlap window 409s instead of clobbering the live leader
(availability-only; the invariant was already safe). As of v0.7.7 the election
closes that window rather than tolerating it — an ex-leader stops instead of
writing — and the same precondition now guards the lease writes themselves.

### Enable checklist (pre-flight over live data — all gates, in order)
1. `UPGRADE_FSM_ENABLED=true` on the operator Deployment AND per-CR
   `spec.upgradePolicy.enabled=true`.
2. Universe operator ClusterRole extended with the pre-flight grants (pods +
   persistentvolumeclaims create/delete, `snapshot.storage.k8s.io` volumesnapshots).
3. A real CSI driver + `VolumeSnapshotClass` in each target namespace.
4. **No Service selector AND no egress-allow NetworkPolicy selects the pre-flight
   pod** in each target namespace — by `podSelector: {}` (namespace-wide) OR by
   ANY label the pre-flight pod carries. Label minimization drops name/instance +
   component/part-of/version, so the only labels left to select on are
   `managed-by` + the two `preflight-*` keys (which no production Service/policy
   selects); the residual is the namespace-wide `podSelector: {}` egress-allow,
   which labels cannot escape (the egress-additivity gate).
5. Live real-namespace egress smoke: confirm the candidate pod boots to ready
   with NO live side effect (no external-DB migration, S3 push, KMS write, IAM
   register) — the one thing unit tests cannot prove.

RED RE-REVIEWS the pre-flight labels + orphan GC before enable.

Test count: 286 → 293 lib tests (+2 controllers::upgrade
[`preflight_labels_exclude_the_service_selector_keys`,
`preflight_pod_does_not_automount_the_sa_token`] + 5 controllers::service
[orphan-GC decision: reclaim-no-inflight, keep-current/reclaim-superseded,
rolling-is-orphan, fail-closed-unknown, grace-window]; extended
`preflight_resources_carry_the_owner_and_label` to assert selector-key ABSENCE;
0 regressions). fmt-clean + clippy-clean (the 4 pre-existing
datastore.rs/manifests.rs warnings on origin/main are untouched). CRD schema
unchanged (no bundle regen).

**Descriptive-label minimization (MEDIUM-1, pre-enable)**. The name/instance
strip closed one selector; the SHARED `component`/`part-of`/`version` keys were
still on the pre-flight pod and re-open the SAME HIGH-1/HIGH-2 via a
descriptive-label selector (a `part-of`-keyed discovery Service or egress-allow —
enable-gate #4's old `podSelector: {}`-only wording did NOT catch these). Fixed
by minimizing at the source (`build_preflight_inputs` → `managed_by_labels()`)
plus a structural strip in `pf_labels` (drops component/part-of/version alongside
name/instance) — the same double belt red credited for name/instance. The
pre-flight set is now EXACTLY `{managed-by, preflight-of, preflight-target}`
(asserted by `labels.len() == 3` per resource). The netpol test was renamed
`preflight_netpol_denies_egress_absent_an_additive_allow_and_selects_only_the_preflight_pod`
(the empty egress rule set denies egress only ABSENT an additive allow — a policy
SHAPE assertion, not the emergent zero-egress guarantee). No new test functions
(existing test strengthened + one renamed) → count stays 293; fmt-clean +
clippy-clean (my files add zero warnings; the 4 pre-existing
datastore.rs/manifests.rs warnings are untouched). CRD schema unchanged. Gate
stays OFF; the remaining enable-gates are pure live-cluster smokes (CSI
round-trip, real-namespace egress, universe RBAC).

## v0.7.9 — securityContext + enableServiceLinks passthrough (Service AND Datastore)

Three optional, backward-compatible fields let a hardened fleet workload port to
an App CR faithfully instead of silently downgrading its posture:
`securityContext` (pod-level: runAsNonRoot/runAsUser/runAsGroup/fsGroup/
seccompProfile), `containerSecurityContext` (main container:
readOnlyRootFilesystem/allowPrivilegeEscalation/capabilities/runAsNonRoot/
runAsUser), and `enableServiceLinks` (the object store sets it `false`; k8s's
default-`true` injects `*_SERVICE_HOST/PORT` env that aborts the s3 flag parser
on restart).

BOTH workload paths render them, through ONE shared helper
(`manifests::pod_security_context`, which folds the legacy top-level `fsGroup`
into the structured pod securityContext — structured `fsGroup` wins):

- **Service** (`ServiceSpec`, flattened by `AppSpec`) — rendered onto the
  Deployment's PodSpec + main container.
- **Datastore** (`DBSpec`, projected from `AppSpec` via a serde round-trip for
  role `sql`/`kv`/`docdb`/`s3`/`datastore`/`managedDatabase`) — `DBSpec` gained
  the SAME three fields so the projection preserves them (previously the
  round-trip silently dropped them: the App CRD *accepted+stored* the fields, but
  `DBSpec` did not model them, so the render never saw them — the s3/SeaweedFS
  object store lost its `enableServiceLinks: false`). `build_datastore_workload`
  renders the container securityContext on the MAIN engine container only (a
  replication/WAL sidecar keeps its writable rootfs) and the folded pod
  securityContext + enableServiceLinks on the PodSpec.

All three are `Option` + `skip_serializing_if`, so a workload that omits them
renders a byte-identical Deployment/StatefulSet. The App reconcile validates
`seccompProfile` at the boundary (type `Localhost` requires `localhostProfile`,
unknown types rejected) so an apiserver-invalid profile degrades cleanly instead
of hot-looping on a rejected apply. CRD bundles regenerated for all four
universes (pure insertion — the six DBSpec CRDs gain the schema, Service/App
unchanged; zero deletions, cmp-verified against a clean regen).

## Delivery in cloud, domain in operator (GitSource retired)
The `GitSource` controller is gone — `controllers/gitsource.rs`, `GitSourceSpec`/
`Status`, the `run_all` wiring, the `/reconcile` webhook + `reconcile_now` Notify,
and the CRD install. The cloud `/v1/deploy` engine (embedded gitops-engine) is the
ONE git→App-CR delivery host; the operator keeps the DOMAIN half — App CR →
Deployment/Service/… — plus `ImageUpdate` (registry→git tag bumps). One way:
delivery in cloud, domain in the operator. (CRD-bundle regen to drop `gitsources`
and the cluster CRD prune are deploy-time follow-ons.)

## v0.7.7 — the election is safe at `replicas > 1` (coordination correctness)

Three defects in leader election, all latent at the shipped `replicas: 1` +
`strategy: Recreate` and all fatal the day anyone scales it. None were firing:
the live pod had logged `Lost leader lease` zero times.

### 1. Losing the lease did not stop the controllers

`run_all_controllers` blocked until `leader_flag` went true and then ran ~30
controllers forever, **never re-reading it**. Only `run_service_controller` and
`run_preflight_gc` were passed the flag. On lease loss the other 28 —
`datastore`, `gateway`, `app`, `dns`, `ingress`, `gitsource`, … — kept
reconciling and writing beside the new leader.

Fixed at ONE seam, not 30. `main.rs` already hosts the election and every
controller in a single `tokio::select!`, so the election's `run` **returning** is
enough: `select!` drops the controllers with it, and main then exits for the
Deployment to restart into a clean election. `LeaderElection::run` now returns on
loss instead of looping. No controller gained a flag check and none needs one —
threading a flag into ~30 call sites is fail-OPEN (every future controller must
remember), while a process that has stopped cannot write at all. `apply.rs` was
considered as the seam and rejected: 27 write sites bypass it, and `install.rs`
uses it from the CLI verb path where there is no leader.

Lease loss ends the process rather than pausing for re-acquisition: fewer moving
parts (the `select!` + Deployment restart both already exist), and fail-closed by
construction. Recovery is fast — a container restart keeps the pod name, so the
identity is unchanged and `holder == self` renews immediately rather than waiting
out the 30s expiry.

### 2. Lease takeover was not compare-and-swap

`try_acquire_or_renew` patched an expired lease with `Patch::Merge` and **no
`resourceVersion` precondition**. Two contenders could both observe the expiry,
both patch, and both conclude they led. The **renew** path had the same hole and
was the sharper one: it patches only `renewTime` off a possibly-superseded read,
so an unconditional write extended the NEW holder's lease while reporting success
to the stale one.

Every lease write is now conditional on the `resourceVersion` read in the same
cycle — the primitive already used for Service status writes (LOW-2), applied
through one `write` helper. A 409 means another contender wrote first, so the
cycle reports `Foreign`, never `Held`. The election declines to claim a lease
that carries no `resourceVersion` rather than fall back to an unconditional
write: a live object always has one, and the fallback is exactly the defect.

Blip tolerance came with it. The flag used to drop on the FIRST failed renew,
which — once loss ends the process — would restart the operator on every
apiserver hiccup. A holder now keeps its lease until the window it last renewed
actually closes (`within_lease`), then yields. `within_lease` goes false at
`elapsed == duration` while a contender takes over only at `elapsed > duration`,
so the hold and takeover windows cannot overlap (asserted over every second
across the boundary).

### 3. The lease was never released on shutdown (found while fixing the above)

`shutdown_signal()` was a `select!` **sibling** of `leader_election.run()`. On
SIGTERM the signal arm won the race and dropped the election future, so the
`release` on `shutdown.changed()` was unreachable and `shutdown_tx.send(true)`
fired after there was nobody left to hear it. `release` was dead code in
production: every operator restart left a stale holder and cost the successor the
full 30s lease timeout before it could take over.

Confirmed in the cluster, not just read: the live pod took over at 02:44:12 after
starting at 02:43:42 — a 30s stall — logging `previous=<the old pod name>`
rather than `<none>`, which is only possible if the predecessor never released.

The signal now drives the shutdown channel from a spawned task, so the election
observes it, releases, and returns; its return completes the `select!`. The
1s sleep that used to paper over this is gone. `LEADER_ELECT=false` dev runs stop
on a signal too (that arm awaits the channel instead of `pending()`).

### Tests

`core::leader` drives the real `run` / `try_acquire_or_renew` against a stand-in
apiserver (`mod fake`, axum) implementing the `resourceVersion` semantics the
safety rests on: writes bump the version, a patch pinning a stale one is rejected
409. A barrier releases both contenders only once each has read, making the race
deterministic rather than hoping it lands.

- `two_contenders_racing_one_expired_lease_produce_one_leader` — the split-brain
  property. Without the precondition both patches land and both report `Held`.
- `a_renew_racing_a_takeover_does_not_report_success` — the renew-path hole.
- `losing_the_lease_stops_the_writes_it_was_hosting` — the stop-writing property,
  observed as the cluster sees it: a controller writing to the fake apiserver is
  hosted in a `select!` beside the election, a successor steals the lease, and
  the writes must stop. Without the fix `run` loops, the controller is never
  dropped, and the test hangs to its timeout.
- `the_hold_window_closes_before_the_takeover_window_opens` and the `must_yield`
  cases pin the yield rule (standby waits; holder yields on `Foreign`; holder
  rides out a blip then yields at expiry).

## v0.7.18 — an fsGroup no longer re-chowns the whole volume at every restart

`hanzo-git` (git.hanzo.ai, the canonical forge for the estate) served HTTP 503
for several minutes with its pod in `Init:0/1` while the kubelet logged:

```
Warning  VolumePermissionChangeInProgress  pod/hanzo-git-...
  Setting volume ownership for .../pvc-47211c5d-3183-4583-8357-3a426d93d91e/mount
  is taking longer than expected, consider using OnRootMismatch
```

The Deployment carries `securityContext: {fsGroup: 1000}` over a 250Gi PVC
holding a git forge — millions of tiny loose objects. K8s defaults
`fsGroupChangePolicy` to `Always`, so the kubelet recursively chowned EVERY file
before the container could start, and each ReplicaSet roll restarted the walk
from zero. The cost is paid on every restart forever, so any service whose volume
grows large enough becomes effectively un-restartable.

`OnRootMismatch` makes the kubelet check only the volume ROOT's ownership and
skip the walk when it already matches — minutes to milliseconds for a volume that
has been mounted before.

### The field

`crd_types::FsGroupChangePolicy` is a closed enum (`Always` | `OnRootMismatch`)
on `PodSecurityContext`, so the CRD schema carries `enum: [Always,
OnRootMismatch]` and a typo is refused at admission rather than stored and later
rejected by the apiserver on the pod apply. It is a closed enum, unlike the open
strings used for values WE own (`role`, `strategy`), because the value set is
defined by Kubernetes and cannot grow under us. It rides `DBSpec` too (same
type), so the datastore projection carries it without further change.

### The default is `OnRootMismatch`, not the k8s `Always`

An `fsGroup` is declared here for exactly one reason — a non-root image must
write a persistence PVC — so the population that sets it IS the population of
long-lived volumes `Always` degrades without bound, and it degrades silently
until it takes an outage. Opt-in would mean every such service pays one outage
first. 14 of 79 live App CRs declare an fsGroup; every one of them mounts a PVC.
The changeover is cheap, not a slow roll: `Always` has already left the volume
root owned by the fsGroup, so the first roll under `OnRootMismatch` matches on
the root check and skips the walk.

What it gives up: `Always` also repairs files DEEP in a volume whose ownership
drifted (a restore that dropped root-owned files in). That is not a property a
workload should depend on, and it stays one explicit field away.

Applied in the ONE fold both workload paths share
(`manifests::pod_security_context` — Deployment via `service.rs`, StatefulSet via
`datastore.rs`), gated on an fsGroup actually being in effect: with no fsGroup
the kubelet never chowns, so no policy is emitted and the PodSpec is
byte-identical. 65 of 79 App CRs render unchanged.

Test count: 351 → 359 lib tests (+3 manifests fold/default/explicit-override,
+2 crd_types conversion + closed-set, +1 service Deployment incident shape,
+1 datastore StatefulSet, +1 app end-to-end from the CR wire shape; 3 existing
exact-shape assertions updated to the new shape). Zero new fmt/clippy warnings.

### Also in this release

- **The App CRD generator emits what the fleet runs.** f722cd8 declared the eight
  role-specific App spec fields and dropped `x-kubernetes-preserve-unknown-fields`
  BY HAND on `k8s/crds/all-hanzo.ai.yaml` — the generator still emitted the flag
  and none of the declarations, so the next `generate-crd-yaml` would silently
  revert a fleet-down fix. Moved into `harden_app_crd`. Verified against the live
  fleet first: 80 App CRs, 34 distinct spec keys, 7 undeclared by the derived
  schema — all covered by the eight, so dropping the flag prunes nothing. The
  regenerated `all-hanzo.ai.yaml` is semantically identical to the hand-edited
  file it replaces; lux/zoo/osage get the same shape.
- **`env_is_carried_to_main_container` is green again.** The gateway-503
  regression test asserted `env.len() == 1` and went red when `build_container`
  began deriving `HANZO_VERSION` (6a237ee). The count was never the invariant; it
  asserts the declared var by name now. No workflow runs `cargo test`, which is
  why it sat red on main.

### Not fixed here — the deployed CRD is a different artifact

`k8s/crds/all-*.yaml` is NOT what the cluster runs. The live `apps.hanzo.ai` CRD
is labelled `app.kubernetes.io/managed-by: universe` and comes from
`hanzoai/universe` `infra/k8s/operator/crds.yaml`, which models 31 spec
properties, still carries `x-kubernetes-preserve-unknown-fields: true`, and
declares NO `securityContext` at all. So on the live cluster today a CR author
cannot express `fsGroupChangePolicy` — or `securityContext`,
`containerSecurityContext`, `enableServiceLinks`, `nodeSelector`, `tolerations`,
`priorityClassName`, `upgradePolicy` — and gets NO error, because an unmodeled
field is silently pruned on write. Shipping this operator version is necessary
but not sufficient: the universe CRD must be refreshed from this generator for
the field to reach a pod.

## v0.7.19 — `replicate.yml` is marshalled, not formatted

The config that took hanzo.chat to 503 was assembled with `format!` and
hand-counted `\x20` column prefixes. Two commits on 2026-07-28 fixed the
indentation (246bcdc) and made the test a parse rather than a substring
(1bbe9a8), and both were right — but the emitter was still text, so the bug
class was intact. MEASURED on that tree: a CR whose `s3Path` is `chat: prod`
(or whose `dataDir` contains ` #`, or whose `pattern` is an unquoted glob)
renders

```
mapping values are not allowed in this context at line 8 column 19
```

— the SAME error, from the SAME file, that killed the `replicate-restore` init
container. Nothing about the age stanza was special; it was the first value to
hit a YAML indicator.

`replicate.yml` is now a `mod replicate_yml` of `Serialize` structs
(`Config`/`Db`/`Replica`/`Age`) handed to `serde_yaml::to_string`. Indentation
and quoting belong to the emitter. `age` is `Option<Age>` on `Replica`, so
identities/recipients are children of `age` by TYPE — there is no arrangement of
whitespace that can un-nest them, and a plaintext bucket still gets no stanza
(the field is skipped when `None`, which is what keeps a plaintext replica
writable). The one hand-written line left is the `#` header comment, which
serde_yaml cannot emit; it is prepended and is not structure.

Semantically identical to what is running: the rendered document for chat's live
`spec.persistence` parses to exactly the same value as the live
`chat-replicate-config` ConfigMap (compared as parsed YAML, not as bytes). The
BYTES differ — serde_yaml writes block sequences at the parent's indentation —
which is the point: nobody counts columns anymore.

### The tests

- `hostile_values_survive_as_strings` / `..._in_dir_mode` — the bug class. A
  `: ` in an s3 path, a ` #` in a data dir, a bucket literally named `yes`, and
  the `**/*.db` glob all round-trip as the strings they are. Both were RED on the
  previous tree with the verbatim production error; that is the negative control
  for the emitter change.
- `the_config_parses_and_the_age_keys_resolve_under_the_replica` (from 1bbe9a8)
  is the nesting contract and still guards the new representation. Negative
  control run: adding `#[serde(flatten)]` to `Replica::age` makes
  identities/recipients escape up to the replica — a document that parses and is
  still the broken config — and turns 3 tests red, naming the shape. Reverted.
- The remaining exact-column substring assertions were deleted, not adjusted.
  They pinned a fiction once the emitter owned indentation, and two ways to read
  one document is one too many. Every assertion about `replicate.yml` now goes
  through a parse, the way `replicate` reads it.

`resolved_persistence` lost its dead `name` parameter (unused since age
defaulting was removed in 51c1b32) — the only `unused variable` warning on main.

328 lib tests, 0 failed. fmt clean. Clippy: identical to main minus that one
warning; zero new.

### Exposure — who else has a `replicate:` block

The CR field is `spec.persistence`; the operator emits `<name>-replicate-config`
only when `enabled: true`. Live on do-sfo3-hanzo-k8s:

| service | enabled | ageSecret | operator-emitted |
|---|---|---|---|
| `hanzo/chat` | true | `chat-replicate-age` | yes — the only one carrying an age stanza |
| `hanzo/dataroom` | true | none | yes — plaintext replica |
| `hanzo/hanzo-app` | **false** | `hanzo-app-replicate-age` | no — its ConfigMap is STALE residue |
| `hanzo/playground` | *(no CR)* | — | no — hand-written, a different schema |

Two follow-ons this does not touch:

- `hanzo-app-replicate-config` and `playground-replicate-config` are ConfigMaps
  the operator does not own and will never reconcile or prune. `hanzo-app`'s was
  written by an older operator (its header still says "age-encrypted", a string
  this code has not emitted since 51c1b32); `playground`'s is a hand-authored
  document with keys the operator has never generated (`sync-interval`,
  `retention`, `on-restore`, age identities as FILE PATHS). Either they are dead
  and should be deleted, or something reads them and it is not declared anywhere.
- The cold-start gate named in the chat/hanzo-app CR comments is still open:
  the snapshots in these buckets are PLAINTEXT while `Replica.OpenLTXFile`
  decrypts unconditionally whenever identities are set. Restore onto an EMPTY
  PVC still fails on `age decrypt: unexpected intro "LTX1"`. Correct YAML does
  not fix that; it is a `hanzoai/replicate` change or a bucket cleanup.
