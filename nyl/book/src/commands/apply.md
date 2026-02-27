# apply

Apply rendered manifests to the Kubernetes cluster with release tracking.

## Synopsis

```bash
nyl apply [OPTIONS] <FILE>
```

## Description

The `apply` command renders manifests, applies them with server-side apply, and tracks release state.
For shared rendering behavior and namespace resolution details, see
[Rendering Pipeline](./rendering-pipeline.md).

## Arguments

- `<FILE>` - Path to the manifest file to apply (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `-p, --profile <PROFILE>` - Profile to use for rendering. If omitted, Nyl tries `default`; if profiles exist but `default` is missing, apply fails with an error.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

### Release Options

- `--name <NAME>` - Release name (required if no NylRelease in file)
- `--namespace <NAMESPACE>` - Release namespace (required if no NylRelease in file)
- `--append-release` - Merge current resources with the previous deployed revision and skip pruning removed resources
- `--no-release` - Apply resources without creating release revisions, without release metadata, and without pruning

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use

## Examples

### Basic Apply

```bash
# Apply a manifest file
nyl apply manifest.yaml

# Apply with specific profile
nyl apply -p production manifest.yaml

# Apply only top-level ConfigMap resources
nyl apply --only-source-kind ConfigMap manifest.yaml

# Apply only final rendered Deployments
nyl apply --only-kind Deployment manifest.yaml
```

### Release Management

```bash
# Apply with explicit release name (overrides NylRelease if present)
nyl apply --name my-release --namespace default manifest.yaml

# Use different Kubernetes context
nyl apply --context production manifest.yaml
```

### Dry Run

Use `nyl diff` to preview changes before running `nyl apply`.

### No Release Mode

```bash
# Apply resources without release tracking or pruning
nyl apply --no-release manifest.yaml
```

## Notes

- Nyl processes single files only. Directory paths are not supported.
- A `NylRelease` resource in the manifest provides release metadata automatically.
- Release state is tracked in Kubernetes Secrets in the release namespace.
- `--no-release` disables release tracking entirely. In this mode, `nyl` cannot compute or prune resources removed from subsequent applies.
- See [Rendering Pipeline](./rendering-pipeline.md) for namespace resolution and filter semantics.
