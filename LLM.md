# Hanzo Operator — AI-Friendly Guide

## What
Unified Kubernetes operator for the Hanzo AI platform. Manages all production infrastructure across 6 K8s clusters declaratively via 7 CRDs under `hanzo.ai/v1alpha1`.

## Tech Stack
- Go 1.25, Kubebuilder v4, controller-runtime v0.23.1
- k8s.io/* v0.35.0
- Image: `ghcr.io/hanzoai/operator:latest`
- Runs in `hanzo-operator-system` namespace

## 7 CRDs

| CRD | Short | Creates |
|-----|-------|---------|
| **HanzoService** | `hsvc` | Deployment, Service, Ingress, HPA, PDB, NetworkPolicy, KMSSecret |
| **HanzoDatastore** | `hds` | StatefulSet, headless Service, PVC, CronJob (backup), KMSSecret |
| **HanzoGateway** | `hgw` | Deployment, Service, ConfigMap (KrakenD), Ingress |
| **HanzoMPC** | `hmpc` | StatefulSet, headless Service, Dashboard Deployment, Cache Deployment, Ingress |
| **HanzoNetwork** | `hnet` | StatefulSet (validators), Deployments (bootnode/indexer/explorer/bridge), Services, ConfigMaps |
| **HanzoIngress** | `hing` | Multiple Ingress resources with cert-manager TLS |
| **HanzoPlatform** | `hplat` | Child CRDs (all of the above) |

## Layout
```
api/v1alpha1/         # CRD types (8 files + generated deepcopy)
cmd/main.go           # Manager entry point
internal/
  controller/         # 7 reconcilers + predicates + helpers
  manifests/          # K8s object builders (builder, labels, mutate)
  status/             # Condition management
  metrics/            # Prometheus metrics
  config/             # Feature gates
config/
  crd/bases/          # Generated CRD YAML (13k lines)
  rbac/               # ClusterRole, bindings
  manager/            # Deployment template
  samples/            # Sample CRs for all 7 CRDs
```

## Key Patterns
- **Predicate filtering**: `createOrUpdatePred`, `updateOrDeletePred`, `statusChangePred`
- **ctrl.CreateOrUpdate** with `MutateFuncFor` for idempotent reconciliation
- **Owner references** for GC cascade
- **Phase lifecycle**: Pending → Creating → Running → Degraded → Deleting
- **KMSSecret delegation**: Creates `kms.hanzo.ai/v1alpha1 KMSSecret` CRs that the existing KMS operator reconciles

## Build
```bash
make manifests generate  # Regen CRDs + deepcopy
make build              # Local binary
make docker-build       # Docker image
docker buildx build --platform linux/amd64 --push -t ghcr.io/hanzoai/operator:latest .
```

## Deploy
Universe manifests at `~/work/hanzo/universe/infra/k8s/hanzo-operator/`.
```bash
kubectl apply -k universe/infra/k8s/hanzo-operator/
```

## Stats
- 28 Go source files, ~7,400 lines
- 7 CRD definitions (13,022 lines YAML)
- RBAC: core, apps, autoscaling, batch, networking, policy, hanzo.ai/*, kms.hanzo.ai/*
