---
title: 'CLI-First Workflows'
---

The CLI workflow is useful when you want direct feedback from Nyl before involving a GitOps controller. It is also the right tool for bootstrapping, debugging, test clusters, and one-off operational checks.

## Inspect Rendered YAML

Render an input file locally:

```bash
nyl render -p dev apps/web.yaml
```

Filter the rendered output when you only need part of the result:

```bash
nyl render -p dev --only-kind Deployment,Service apps/web.yaml
```

Use offline mode when you want repeatable rendering without Kubernetes discovery:

```bash
nyl render \
  --profile dev \
  --offline \
  --kube-version 1.30.0 \
  --kube-api-versions v1,apps/v1 \
  apps/web.yaml
```

## Preview Cluster Changes

Use `nyl diff` to compare rendered manifests with live cluster state:

```bash
nyl diff -p dev apps/web.yaml
```

For a shorter signal in automation:

```bash
nyl diff -p dev --summary apps/web.yaml
```

## Apply Directly

Use `nyl apply` when direct application is intentional, such as during bootstrap or test-cluster setup:

```bash
nyl apply -p dev apps/web.yaml
```

`nyl apply` tracks release state in Kubernetes Secrets unless `--no-release` is used. That release state allows Nyl to understand what it previously applied and helps with pruning.

## Typical Uses

- Bootstrap ArgoCD or other cluster prerequisites before GitOps is available.
- Reproduce a CI render locally while debugging a failed rendered-manifest job.
- Preview generated Helm resources before committing rendered YAML.
- Test component changes against an ephemeral cluster.
- Apply temporary environments where a full GitOps loop would be unnecessary.
