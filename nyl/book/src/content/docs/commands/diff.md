---
title: 'diff'
---

Show the difference between rendered manifests and the current cluster state.

## Synopsis

```bash
nyl diff --target <TARGET> [OPTIONS] <FILE>
```

## Description

The `diff` command renders manifests, compares them with live cluster state, and prints changes.
For shared rendering behavior and namespace resolution details, see
[Rendering Pipeline](/nyl/commands/rendering-pipeline/).

## Arguments

- `<FILE>` - Path to the manifest file to diff (required)

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

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use instead of `Cluster.spec.live.context`

### Diff Options

- `--summary` - Show summary only (counts, no detailed diff)
- `--mode <MODE>` - Diff mode: `normalized` (default) or `raw`
  - `normalized`: Uses server-side apply to filter server defaults (like kubectl diff)
  - `raw`: Compares raw manifests without server normalization
- `--append-release` - Preview diff as if current manifests were merged with the previous deployed release

## Examples

### Basic Diff

```bash
# Show diff for a manifest file
nyl diff --target production manifest.yaml

# Diff another target
nyl diff --target staging manifest.yaml

# Diff only top-level ConfigMap resources
nyl diff --target production --only-source-kind ConfigMap manifest.yaml

# Diff only final rendered Deployments
nyl diff --target production --only-kind Deployment manifest.yaml
```

### Summary Mode

```bash
# Show only the summary
nyl diff --target production --summary manifest.yaml
```

### Diff Modes

```bash
# Normalized mode (default) - filters server defaults
nyl diff --target production --mode normalized manifest.yaml

# Raw mode - shows all differences including server defaults
nyl diff --target production --mode raw manifest.yaml
```

### Release Management

```bash
# Diff with explicit release name
nyl diff --target production --name my-release --namespace default manifest.yaml

# Use different Kubernetes context
nyl diff --target production --context admin@production manifest.yaml
```

## Output

The diff command shows:
- **Green (+)**: Lines that will be added
- **Red (-)**: Lines that will be removed
- **Summary**: Count of resources to create, update, or delete

## Notes

- Nyl processes single files only. Directory paths are not supported.
- A `NylRelease` resource in the manifest provides release metadata automatically.
- Normalized mode is recommended for most use cases as it matches kubectl diff behavior.
- If no previous release state exists, diff still compares desired resources against live state but cannot determine prune candidates; a warning is shown and `to delete` remains incomplete.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution and filter semantics.
