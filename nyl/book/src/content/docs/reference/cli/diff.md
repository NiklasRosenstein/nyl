---
title: 'diff'
---

Show the difference between rendered manifests and the current cluster state.

## Synopsis

```bash
nyl diff [--target <TARGET>] [OPTIONS] <FILE>
```

## Description

The `diff` command renders manifests, compares them with live cluster state, and prints changes.
It compares exactly what [`apply`](/nyl/reference/cli/apply/) would apply and
prune. See [Rendering Pipeline](/nyl/kubernetes/pipeline/#the-releases-application)
for the behavior `render`, `diff`, and `apply` share.

## Arguments

- `<FILE>` - Path to the manifest file to diff (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `--target <TARGET>` - DeploymentTarget whose Cluster supplies values, capabilities, destination identity, and the default kube context. Optional when exactly one target is configured; required when several are available.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations
- `--release-only` and `--allow-secret-inputs` - As for [`apply`](/nyl/reference/cli/apply/#common-options).

### Release Options

- `--name <NAME>` - Release name (required if no Release in file)
- `--namespace <NAMESPACE>` - Release namespace, and the namespace of resources that set none (required if no Release in file)

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use instead of `Cluster.spec.live.context`

### Diff Options

- `--summary` - Show summary only (counts, no detailed diff)
- `--mode <MODE>` - Diff mode: `normalized` (default) or `raw`
  - `normalized`: Uses server-side apply to filter server defaults (like kubectl diff)
  - `raw`: Compares raw manifests without server normalization
- `--append-release` - Preview diff as if current manifests were merged with the previous deployed release
- `--confirm-prune` - Show resources whose sync option is `Prune=confirm` as deleted, as `apply --confirm-prune` deletes them. Without it they are listed with `!` as kept, like resources with `Prune=false`

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

Each resource is listed with what `apply` would do to it: `+` create, `~`
update (followed by the unified diff), `-` prune, `!` keep although no longer
rendered, and `=` leave unchanged. The summary counts each. Here the Release
raised its replicas and dropped its `web-jobs` namespace, whose managed
Namespace awaits prune confirmation:

```console
$ nyl diff --target production applications/web/release.yaml
~ Deployment web/web
@@ -5,7 +5,7 @@   namespace: web
 spec:
   progressDeadlineSeconds: 600
-  replicas: 2
+  replicas: 3
   revisionHistoryLimit: 10
   selector:
     matchLabels:

- ConfigMap web-jobs/cleanup-schedule
! Namespace web-jobs (kept: Prune=confirm needs --confirm-prune)

= ConfigMap web/web-config
= Namespace web

Summary: 0 to add, 1 to modify, 1 to delete, 2 unchanged, 1 kept
```

Before the first apply, every resource is new, including the managed Namespaces
the Release's Application deploys:

```console
$ nyl diff --target production applications/web/release.yaml
+ ConfigMap web/web-config
+ ConfigMap web-jobs/cleanup-schedule
+ Deployment web/web
+ Namespace web-jobs
+ Namespace web

Summary: 5 to add, 0 to modify, 0 to delete, 0 unchanged
```

## Notes

- Nyl accepts one entry file. `Release.spec.include` can attach additional relative manifest files and glob matches; directory arguments are not supported.
- A `Release` resource in the manifest provides release metadata automatically.
- Normalized mode is recommended for most use cases as it matches kubectl diff behavior.
- If no previous release state exists, diff still compares desired resources against live state but cannot determine prune candidates; a warning is shown and `to delete` remains incomplete.
- See [Rendering Pipeline](/nyl/kubernetes/pipeline/#namespaces-in-diff-and-apply) for namespaces and filter semantics.

## Manifest validation

Use `--validate` to run project-configured validators. See
[Manifest validation](/nyl/kubernetes/validation/manifest-validation/) for automatic validation, captured
CRD schemas, inherited Cluster contracts, and offline schema vendoring.
