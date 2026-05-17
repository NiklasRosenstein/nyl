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
- `-p, --profile <PROFILE>` - Profile to use for rendering. If omitted, Nyl tries `default`; if profiles exist but `default` is missing, rendering fails with an error.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

### Offline Mode Options

- `--offline` - Skip Kubernetes discovery and use CLI/config-provided API information (useful for CI/CD)
- `--kube-version <KUBE_VERSION>` - Kubernetes version for Helm templating. Overrides `nyl.toml`.
- `--kube-api-versions <KUBE_API_VERSIONS>` - Kubernetes API versions for Helm, comma-separated or repeated. Overrides `nyl.toml`.

## Examples

### Basic Rendering

```bash
# Render a manifest file
nyl render manifest.yaml

# Render with specific profile
nyl render -p production manifest.yaml

# Filter top-level input resources
nyl render --only-source-kind ConfigMap manifest.yaml

# Filter by full apiVersion/kind
nyl render --only-source-kind apps/v1/Deployment manifest.yaml

# Filter final rendered output kinds
nyl render --only-kind Deployment,Service manifest.yaml
```

### Offline Mode

```bash
# Render in offline mode using CLI-provided Kubernetes target metadata
nyl render --offline --kube-version 1.30 --kube-api-versions v1,apps/v1 manifest.yaml

# Render in offline mode using [project.kubernetes] or [profile.<name>.kubernetes]
nyl render --offline -p production manifest.yaml
```

### Advanced Options

```bash
# Limit recursive expansion depth
nyl render --max-depth 5 manifest.yaml

# Track parent resources in annotations
nyl render --track-parent manifest.yaml

# Combine options
nyl render -p staging --max-depth 3 --track-parent manifest.yaml
```

## Notes

- Nyl processes single files only. Directory paths are not supported.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution, filter semantics, and online/offline behavior.
- `ApplicationGenerator` source resolution first honors `NYL_APPGEN_REPO_PATH_OVERRIDE`, then tries to reuse the current local Git checkout when `repoURL` matches a local remote and `targetRevision` is `HEAD` or the current branch, then falls back to ArgoCD checkout reuse and normal Git cache/worktree resolution.
- Local ApplicationGenerator testing override: set `NYL_APPGEN_REPO_PATH_OVERRIDE` to a local repository root (or `@git` to auto-detect the Git root from the current `PWD`) to make ApplicationGenerator scan the local filesystem instead of cloning. This affects `render`, `diff`, and `apply` (all use the same render pipeline). Using `@git` outside a Git repository fails with a configuration error.
- ApplicationGenerator discovery semantics: `source.path` scans non-recursively by default; use glob selectors (or `source.paths`) for recursive discovery, and include/exclude patterns match relative paths.
