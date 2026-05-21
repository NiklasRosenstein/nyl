---
title: 'apply'
---

Apply rendered manifests to the Kubernetes cluster with release tracking.

## Synopsis

```bash
nyl apply [OPTIONS] <FILE>
```

## Description

The `apply` command renders manifests, applies them with server-side apply, and tracks release state.
For shared rendering behavior and namespace resolution details, see
[Rendering Pipeline](/nyl/commands/rendering-pipeline/).

## Semantics

After rendering, `nyl apply`:
1. Resolves namespaces for namespaced resources.
2. Sorts resources by apply priority (for example: Namespace, CRD, RBAC, then workloads).
3. Applies resources with server-side apply.
4. Records release state (unless `--no-release` is used).
5. Prunes resources removed from the desired state (default mode only).

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

## Helm Hook Behavior

Resources with both:
- `helm.sh/hook`
- `helm.sh/hook-delete-policy` containing `before-hook-creation`

are deleted first and then applied again, matching hook recreation semantics for that policy.

Other Helm-hooked resources are applied normally.

## Release and Pruning Semantics

- In default mode, apply creates a new release revision and marks successful previous revisions as superseded.
- In default mode, prune deletes resources that existed in the previous revision but are no longer present in the new desired state.
- With `--append-release`, the new release is built from union(previous, current) where current resources win on overlap; pruning is skipped.
- `--append-release` requires the previous release (if present) to be in `Deployed` state.
- If no previous release exists, `--append-release` behaves like an initial apply.
- With `--no-release`, no revisions are written and no pruning is performed.

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
- `--no-release` conflicts with `--append-release`, `--name`, and `--namespace`.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution and filter semantics.
