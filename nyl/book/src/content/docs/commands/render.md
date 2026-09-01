---
title: 'render'
---

Render Kubernetes manifests from nyl components and templates.

## Synopsis

```bash
nyl render [OPTIONS] <FILE>
```

## Description

The `render` command generates Kubernetes manifests and writes final YAML to stdout.
For detailed shared pipeline behavior (also used by `diff` and `apply`), see
[Rendering Pipeline](/nyl/commands/rendering-pipeline/).

## Arguments

- `<FILE>` - Path to the manifest file to render (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `--target <TARGET>` - GitOpsTarget whose Cluster, values, and Kubernetes capabilities are used for rendering. Optional for base rendering.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations
- `--refresh` - Bypass cached rendering results and replace successful entries.
- `--no-cache` - Perform no persistent cache reads or writes.

### Offline Mode Options

- `--offline` - Skip Kubernetes discovery and use the target Cluster or explicit API information.
- `--kube-version <KUBE_VERSION>` - Kubernetes version for a targetless offline render.
- `--kube-api-versions <KUBE_API_VERSIONS>` - Kubernetes API versions for a targetless offline render, comma-separated or repeated.

## Examples

### Basic Rendering

```bash
# Render a manifest file
nyl render manifest.yaml

# Render with a configured target
nyl render --target production manifest.yaml

# Filter top-level input resources
nyl render --only-source-kind ConfigMap manifest.yaml

# Filter by full apiVersion/kind
nyl render --only-source-kind apps/v1/Deployment manifest.yaml

# Filter final rendered output kinds
nyl render --only-kind Deployment,Service manifest.yaml
```

### Offline Mode

```bash
# Targetless offline render with explicit Kubernetes capabilities
nyl render --offline --kube-version 1.30 --kube-api-versions v1,apps/v1 manifest.yaml

# Target-aware offline render using the Cluster's committed capabilities
nyl render --target production --offline manifest.yaml
```

### Advanced Options

```bash
# Limit recursive expansion depth
nyl render --max-depth 5 manifest.yaml

# Track parent resources in annotations
nyl render --track-parent manifest.yaml

# Force source resolution and manifest expansion to run again
nyl render --refresh manifest.yaml

# Combine options
nyl render --target staging --max-depth 3 --track-parent manifest.yaml
```

## Notes

- Nyl accepts one entry file. `Release.spec.include` can attach additional relative manifest files and glob matches; directory arguments are not supported.
- Expansion failures report the originating manifest path, document number, and recursive Component or HelmChart resource chain. This provenance is internal and is not added to rendered Kubernetes objects.
- Bundle and Helm rendering use the shared content-addressed render cache. `diff` and `apply` reuse the same desired-manifest artifacts before performing their live-cluster operations.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution, filter semantics, and online/offline behavior.
- `ApplicationGenerator` source resolution first honors `NYL_APPGEN_REPO_PATH_OVERRIDE`, then tries to reuse the current local Git checkout when `repoURL` matches a local remote and `targetRevision` is `HEAD` or the current branch, then falls back to ArgoCD checkout reuse and normal Git cache/worktree resolution.
- Local ApplicationGenerator testing override: set `NYL_APPGEN_REPO_PATH_OVERRIDE` to a local repository root (or `@git` to auto-detect the Git root from the current `PWD`) to make ApplicationGenerator scan the local filesystem instead of cloning. This affects `render`, `diff`, and `apply` (all use the same render pipeline). Using `@git` outside a Git repository fails with a configuration error.
- ApplicationGenerator discovery semantics: `source.path` scans non-recursively by default; use glob selectors (or `source.paths`) for recursive discovery, and include/exclude patterns match relative paths.
