---
title: 'apply'
---

Apply rendered manifests to the Kubernetes cluster with release tracking.

## Synopsis

```bash
nyl apply --target <TARGET> [OPTIONS] <FILE>
```

## Description

The `apply` command renders manifests, applies them with server-side apply, and tracks release state.
For shared rendering behavior and namespace resolution details, see
[Rendering Pipeline](/nyl/commands/rendering-pipeline/).

## Arguments

- `<FILE>` - Path to the manifest file to apply (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `--target <TARGET>` - Required GitOpsTarget. Its Cluster supplies values, capabilities, destination identity, and the default kube context.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

### Release Options

- `--name <NAME>` - Release name (required if no NylRelease in file)
- `--namespace <NAMESPACE>` - Release namespace (required if no NylRelease in file)
- `--append-release` - Merge current resources with the previous deployed revision and skip pruning removed resources
- `--no-release` - Apply resources without creating release revisions, without release metadata, and without pruning

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use instead of `Cluster.spec.live.context`

## Examples

### Basic Apply

```bash
# Apply a manifest file
nyl apply --target production manifest.yaml

# Apply another target
nyl apply --target staging manifest.yaml

# Apply only top-level ConfigMap resources
nyl apply --target production --only-source-kind ConfigMap manifest.yaml

# Apply only final rendered Deployments
nyl apply --target production --only-kind Deployment manifest.yaml
```

### Release Management

```bash
# Apply with explicit release name (overrides NylRelease if present)
nyl apply --target production --name my-release --namespace default manifest.yaml

# Use different Kubernetes context
nyl apply --target production --context admin@production manifest.yaml
```

### Dry Run

Use `nyl diff` to preview changes before running `nyl apply`.

### No Release Mode

```bash
# Apply resources without release tracking or pruning
nyl apply --target production --no-release manifest.yaml
```

## Notes

- Nyl processes single files only. Directory paths are not supported.
- A `NylRelease` resource in the manifest provides release metadata automatically.
- Release state is tracked in Kubernetes Secrets in the release namespace. Use [`nyl release`](/nyl/commands/release/) to inspect history or [roll back](/nyl/commands/release/#rollback) to a previous revision.
- `--no-release` disables release tracking entirely. In this mode, `nyl` cannot compute or prune resources removed from subsequent applies.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution and filter semantics.
