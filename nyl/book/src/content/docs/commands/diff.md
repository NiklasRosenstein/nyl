---
title: 'diff'
---

Show the difference between rendered manifests and the current cluster state.

## Synopsis

```bash
nyl diff [OPTIONS] <FILE>
```

## Description

The `diff` command renders manifests, compares them with live cluster state, and prints changes.
For shared rendering behavior and namespace resolution details, see
[Rendering Pipeline](/nyl/commands/rendering-pipeline/).

## Semantics

`nyl diff` compares:
- **Desired state**: the rendered manifests from the current input file
- **Live state**: matching resources currently in the cluster
- **Previous release state** (if available): used to identify prune candidates (`to delete`)

If no previous release state exists, diff still compares desired vs live for existing resources, but it cannot fully determine deletions from earlier revisions.

## Arguments

- `<FILE>` - Path to the manifest file to diff (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `-p, --profile <PROFILE>` - Profile to use for rendering. If omitted, Nyl tries `default`; if profiles exist but `default` is missing, diff fails with an error.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

### Release Options

- `--name <NAME>` - Release name (required if no NylRelease in file)
- `--namespace <NAMESPACE>` - Release namespace (required if no NylRelease in file)

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use

### Diff Options

- `--summary` - Show summary only (counts, no detailed diff)
- `--mode <MODE>` - Diff mode: `normalized` (default) or `raw`
  - `normalized`: Uses server-side apply to filter server defaults (like kubectl diff)
  - `raw`: Compares raw manifests without server normalization
- `--append-release` - Preview diff as if current manifests were merged with the previous deployed release
- `--exit-code` - Exit with code `1` when changes are found, `0` when no changes are found and no errors occurred

## Helm Hook Behavior

Resources with both:
- `helm.sh/hook`
- `helm.sh/hook-delete-policy` containing `before-hook-creation`

are treated as resources that will be recreated on apply. In diff output they are shown as **additions** with the note:
- `(Helm hook will be recreated before apply)`

Other Helm-hooked resources are diffed normally.

## Append-Release Preview Semantics

- With `--append-release`, desired resources are merged with the previous release's resources (set union; current manifests win on overlap).
- If the previous release exists but is not in `Deployed` state, diff fails.
- If no previous release exists, append-release preview behaves like an initial release diff.

## Examples

### Basic Diff

```bash
# Show diff for a manifest file
nyl diff manifest.yaml

# Diff with specific profile
nyl diff -p production manifest.yaml

# Diff only top-level ConfigMap resources
nyl diff --only-source-kind ConfigMap manifest.yaml

# Diff only final rendered Deployments
nyl diff --only-kind Deployment manifest.yaml
```

### Summary Mode

```bash
# Show only the summary
nyl diff --summary manifest.yaml
```

### Diff Modes

```bash
# Normalized mode (default) - filters server defaults
nyl diff --mode normalized manifest.yaml

# Raw mode - shows all differences including server defaults
nyl diff --mode raw manifest.yaml
```

### Release Management

```bash
# Diff with explicit release name
nyl diff --name my-release --namespace default manifest.yaml

# Use different Kubernetes context
nyl diff --context production manifest.yaml
```

## Output

The diff command shows:
- **Green (+)**: Lines that will be added
- **Red (-)**: Lines that will be removed
- **Yellow (~)**: Resources that will be modified
- **Grey (=)**: Resources that are unchanged
- **Summary**: Count of resources to create, update, or delete

## Exit Codes

- `0`: Diff completed without errors, and either no changes were found or `--exit-code` was not set
- `1`: `--exit-code` was set and changes were found
- `2`: Diff encountered errors (for example, normalization failures)

## Notes

- Nyl processes single files only. Directory paths are not supported.
- A `NylRelease` resource in the manifest provides release metadata automatically.
- Normalized mode is recommended for most use cases as it matches kubectl diff behavior.
- In normalized mode, if server-side normalization fails for a resource, Nyl falls back to raw diff for that resource and reports the normalization error.
- If no previous release state exists, diff still compares desired resources against live state but cannot determine prune candidates; a warning is shown and `to delete` remains incomplete.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution and filter semantics.
