---
title: 'apply'
---

Apply rendered manifests to the Kubernetes cluster with release tracking.

## Synopsis

```bash
nyl apply [--target <TARGET>] [OPTIONS] <FILE>
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
- `--target <TARGET>` - DeploymentTarget whose Cluster supplies values, capabilities, destination identity, and the default kube context. Optional when exactly one target is configured; required when several are available.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

### Release Options

- `--name <NAME>` - Release name (required if no Release in file)
- `--namespace <NAMESPACE>` - Release namespace (required if no Release in file)
- `--append-release` - Merge current resources with the previous deployed revision and skip pruning removed resources
- `--no-release` - Apply resources without creating release revisions, without release metadata, and without pruning

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use instead of `Cluster.spec.live.context`
- `--concurrency <N>` - Maximum number of resources applied or pruned at the same time (default: 8; `1` applies serially)

## Apply order and progress

Nyl applies resources in the same order as Argo CD: Namespaces, cluster policy
(NetworkPolicies, ResourceQuotas, LimitRanges, PodDisruptionBudgets),
ServiceAccounts, Secrets and ConfigMaps, storage, CustomResourceDefinitions, RBAC,
Services, workloads, Ingresses, and APIServices, followed by every other kind,
including custom resources and admission webhook configurations. As in Argo CD,
the resources of one kind form a batch that is applied concurrently, up to
`--concurrency` at a time, and each batch finishes before the next starts. Kinds
therefore never race a kind they may depend on, while resources of one kind do
not wait for each other.

Before applying the first resource of an API group that a CustomResourceDefinition
or APIService registered earlier in the same apply, Nyl refreshes API discovery
until the new kinds are served. When several documents describe the same object
(the same group, kind, namespace, and name, in any API version), only the last one
is applied and recorded. A resource that fails to apply does not stop the others;
the release is recorded as failed and the command exits non-zero.

Pruning deletes the resources a release no longer contains concurrently, like Argo
CD. APIServices and admission webhook configurations are deleted last, after every
other pruned resource, so they keep serving and admitting requests while the
resources related to them are removed.

Each resource's outcome (`+` created, `~` updated, `=` unchanged, `✗` failed) is
printed as soon as it completes, in completion order. The summary is followed by
the time spent in API discovery, validation, apply, and release bookkeeping.
Nyl uses aggregated API discovery (two requests) when the API server supports it
and falls back to per-group discovery otherwise; the timing line names the mode.

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
# Apply with explicit release name (overrides Release if present)
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

- Nyl accepts one entry file. `Release.spec.include` can attach additional relative manifest files and glob matches; directory arguments are not supported.
- A `Release` resource in the manifest provides release metadata automatically.
- Release state is tracked in Kubernetes Secrets in the release namespace. Use [`nyl release`](/nyl/commands/release/) to inspect history or [roll back](/nyl/commands/release/#rollback) to a previous revision.
- The release namespace is created when it is missing, before the manifests are
  applied. Nyl does not own that namespace and never prunes it. Other namespaces
  a release writes to, including `spec.additionalNamespaces`, need their own
  `Namespace` manifest.
- `--no-release` disables release tracking entirely. In this mode, `nyl` cannot compute or prune resources removed from subsequent applies.
- See [Rendering Pipeline](/nyl/commands/rendering-pipeline/) for namespace resolution and filter semantics.

## Manifest validation

Use `--validate` to run project-configured validators. See
[Manifest validation](../manifest-validation/) for automatic validation, captured
CRD schemas, inherited Cluster contracts, and offline schema vendoring.
