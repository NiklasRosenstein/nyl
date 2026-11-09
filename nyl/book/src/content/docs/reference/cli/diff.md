---
title: 'diff'
---

Show the difference between rendered manifests and the current cluster state.

## Synopsis

```bash
nyl diff [--target <TARGET> | --context <CONTEXT>] [OPTIONS] <FILE>
```

## Description

The `diff` command renders manifests, compares them with live cluster state, and prints changes.
It compares exactly what [`apply`](/nyl/reference/cli/apply/) would apply and
prune. See [Rendering Pipeline](/nyl/kubernetes-rendering/pipeline/#the-releases-application)
for the behavior `render`, `diff`, and `apply` share.

A diff creates nothing, so a custom resource of a CustomResourceDefinition that is
new in the same manifest cannot be compared with the server. It is listed as added
with the note `CRD will be installed` and is not an error; a custom resource whose
CRD is neither in the manifest nor in the cluster is an error. The namespace such a
resource takes is decided as `apply` decides it
([which kinds are namespaced](/nyl/reference/cli/apply/#which-kinds-are-namespaced)).

## Arguments

- `<FILE>` - Path to the manifest file to diff (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `--target <TARGET>` - DeploymentTarget whose Cluster supplies values, capabilities, destination identity, and the default kube context. Optional when exactly one target is configured; required when several are available. A project without DeploymentTargets uses `--context` instead.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations
- `--release-only` and `--allow-secret-inputs` - As for [`apply`](/nyl/reference/cli/apply/#common-options).

### Release Options

- `--name <NAME>` - Release name (required if no Release in file)
- `--namespace <NAMESPACE>` - Release namespace, and the namespace of resources that set none (required if no Release in file)

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use instead of `Cluster.spec.live.context`. When the Cluster declares a `live.context`, it is used and the current context never is. Without either, the current context is used only when the Cluster's destination server is a concrete URL that can be checked against it; an in-cluster or by-name destination needs `--context`. The context's server must match the declared destination either way. Without GitOps configuration, `--context` selects the cluster instead; see [Without GitOps configuration](/nyl/kubernetes-rendering/pipeline/#without-gitops-configuration).

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

# Diff against a kube context in a project without GitOps configuration
nyl diff --context kind-kind manifest.yaml
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

### Hooks

A [hook](/nyl/reference/cli/apply/#hooks-and-sync-waves) is no resource `apply` adds,
changes, or deletes, so the diff does not list it with the resources. After them, a
section lists the hook runs `apply` would make, in the order they run: the phase, the
sync wave, the object, and its delete policies. A hook that names several phases is
listed once for each. Hooks are in none of the counts and never set the exit status
of `--exit-code`: the exit status is `1` only when a resource would be added, changed,
or deleted, and `2` when the diff itself failed. `--summary` shows the section too.

```console
$ nyl diff --target production applications/web/release.yaml
= Deployment web/web

Hooks that would run:
  ▶ PreSync  wave  -1  Job web/migrate  (delete: HookSucceeded)
  ▶ PostSync wave   0  Job web/smoke-test  (delete: BeforeHookCreation)

Summary: 0 to add, 0 to modify, 0 to delete, 1 unchanged
```

Hooks that name no sync phase are left out of the apply. Helm's `test` hook and the
rollback hooks are listed after the hooks that would run, for information, under
"Hooks left out"; delete hooks (`PreDelete`, `PostDelete`, `pre-delete`,
`post-delete`), which Nyl never runs, get the warning `NYL-KUBERNETES-HOOK-IGNORED`
once for the Release. See
[Hooks left out](/nyl/reference/cli/apply/#hooks-and-sync-waves).

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
- See [Rendering Pipeline](/nyl/kubernetes-rendering/pipeline/#namespaces-in-diff-and-apply) for namespaces and filter semantics.

## Manifest validation

Use `--validate` to run project-configured validators. See
[Manifest validation](/nyl/kubernetes-rendering/validation/manifest-validation/) for automatic validation, captured
CRD schemas, inherited Cluster contracts, and offline schema vendoring.
