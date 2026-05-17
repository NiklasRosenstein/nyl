---
title: 'Rendered Manifest GitOps'
---

The rendered manifest pattern keeps Nyl in CI and keeps the runtime GitOps controller simple. Nyl renders your source manifests into ordinary Kubernetes YAML, and ArgoCD, Flux, or another reconciler syncs that rendered output.

## When to Use It

Use this workflow when you want:

- A plain-YAML contract between generation and deployment.
- ArgoCD or Flux to run without a custom plugin sidecar.
- Rendered output that can be reviewed, diffed, signed, or promoted between environments.
- A low-friction path for teams that already sync directories of Kubernetes manifests.

Use the [ArgoCD CMP integration](/nyl/argocd/overview/) instead when ArgoCD must render directly from Nyl inputs, discover ArgoCD repository secrets, or use `ApplicationGenerator` at sync time.

## Repository Shape

Keep source and rendered output separate:

```text
platform/
├── nyl.toml
├── apps/
│   └── web.yaml
├── components/
└── rendered/
    ├── dev/
    │   └── web.yaml
    └── prod/
        └── web.yaml
```

The `apps/` directory contains the Nyl input. The `rendered/` directory contains generated Kubernetes manifests that a GitOps controller can sync with its normal directory support.

## Render in CI

`nyl render` writes YAML to stdout, so the CI job should redirect output to the rendered manifest location:

```bash
nyl validate --strict
mkdir -p rendered/prod
nyl render -p prod apps/web.yaml > rendered/prod/web.yaml
```

For CI jobs that should not talk to a cluster, use offline rendering:

```bash
nyl render \
  --profile prod \
  --offline \
  --kube-version 1.30.0 \
  --kube-api-versions v1,apps/v1,batch/v1,networking.k8s.io/v1 \
  apps/web.yaml > rendered/prod/web.yaml
```

You can capture the API versions from a representative cluster with `kubectl`:

```bash
kubectl api-versions | paste -sd, -
```

That produces the comma-separated value expected by `--kube-api-versions`. In CI, either run this against the target cluster before rendering or store a checked-in value for each supported cluster version:

```bash
KUBE_API_VERSIONS="$(kubectl api-versions | paste -sd, -)"

nyl render \
  --profile prod \
  --offline \
  --kube-version 1.30.0 \
  --kube-api-versions "$KUBE_API_VERSIONS" \
  apps/web.yaml > rendered/prod/web.yaml
```

Prefer sourcing this list from Kubernetes discovery when possible, because it includes CRDs and aggregated APIs installed in the cluster. A dedicated Nyl utility command would mostly wrap this discovery step; until Nyl has one, `kubectl api-versions` is the most direct source of truth.

Nyl renders one input file per command. For multiple applications, use an explicit script, Makefile target, or CI matrix so each input has a predictable output path.

## Sync Rendered Output

Point ArgoCD, Flux, or another reconciler at the rendered directory. With ArgoCD this is a standard directory Application, not the Nyl CMP:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: web-prod
  namespace: argocd
spec:
  project: default
  source:
    repoURL: https://github.com/example/platform.git
    targetRevision: main
    path: rendered/prod
  destination:
    server: https://kubernetes.default.svc
    namespace: web
  syncPolicy:
    automated:
      prune: true
      selfHeal: true
```

## Review and Promotion

The rendered output can be handled like any other generated artifact:

- Commit rendered files back to the same repository.
- Publish rendered files to a deployment repository.
- Upload rendered files as CI artifacts for a separate promotion job.
- Run policy checks, signing, or drift checks against the rendered YAML.

The important boundary is that the cluster reconciler consumes rendered Kubernetes manifests. Nyl stays in the build path where failures are easier to debug and roll back.
