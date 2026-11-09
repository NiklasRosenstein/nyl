---
title: 'apply'
---

Apply rendered manifests to the Kubernetes cluster with release tracking.

## Synopsis

```bash
nyl apply [--target <TARGET> | --context <CONTEXT>] [OPTIONS] <FILE>
```

## Description

The `apply` command renders manifests, applies them with server-side apply, and tracks release state.
It applies a Release as its Argo CD Application would deploy it from the
`render-tree` output, including managed Namespaces, and prunes as an Argo CD sync
prunes. See [Rendering Pipeline](/nyl/kubernetes-rendering/pipeline/#the-releases-application)
for the behavior `render`, `diff`, and `apply` share.

## Arguments

- `<FILE>` - Path to the manifest file to apply (required)

## Options

### Common Options

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `--target <TARGET>` - DeploymentTarget whose Cluster supplies values, capabilities, destination identity, and the default kube context. Optional when exactly one target is configured; required when several are available. A project without DeploymentTargets uses `--context` instead.
- `--release-only` - Settle the Release alone instead of taking it from the compiled target; see [`render`](/nyl/reference/cli/render/#common-options).
- `--allow-secret-inputs` - Let the project secrets provider and `NYL_*` environment variables affect the applied manifests.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

`--input`, `--inputs`, `--application-group`, `--defaults-only`, and the cache
options work as for [`render`](/nyl/reference/cli/render/#common-options).

### Release Options

- `--name <NAME>` - Release name (required if no Release in file)
- `--namespace <NAMESPACE>` - Release namespace, and the namespace of resources that set none (required if no Release in file)
- `--take-over` - Apply a release an environment owns anyway, recording no owner on the new revision. Without it, `nyl apply` refuses a release owned by a unit of a [direct target](/nyl/kubernetes-gitops/workflow/direct-reconciler/) (`NYL-RELEASE-OWNED-BY-UNIT`); the owner's next reconcile adopts the release again and reverts the change.
- `--confirm-prune` - Also prune resources whose Argo CD sync option is `Prune=confirm`, such as managed Namespaces
- `--append-release` - Merge current resources with the previous deployed revision and skip pruning removed resources
- `--no-release` - Apply resources without creating release revisions, without release metadata, and without pruning
- `--history-limit <N>` - Keep at most `N` release revisions, deleting the oldest after a successful apply; `0` keeps every revision, otherwise `N` must be at least `2`. Overrides `[release] history_limit` in [`nyl.toml`](/nyl/reference/configuration/#release-history)

### Field Ownership Options

- `--handoff[=<MANAGER>]` - Apply as field manager `MANAGER` (default
  `argocd-controller`, Argo CD's server-side apply manager) instead of `nyl`, and
  release the fields earlier applies made as `nyl`. Use it before Argo CD takes
  over resources Nyl applied, and for any apply while Argo CD manages them; see
  [Hand Over to Argo CD](/nyl/kubernetes-gitops/workflow/handoff/). The value
  must be attached with `=`.

### Cluster Options

- `--context <CONTEXT>` - Kubernetes context to use instead of `Cluster.spec.live.context`. When the Cluster declares a `live.context`, it is used and the current context never is. Without either, the current context is used only when the Cluster's destination server is a concrete URL that can be checked against it; an in-cluster or by-name destination needs `--context`. The context's server must match the declared destination either way. Without GitOps configuration, `--context` selects the cluster instead; see [Without GitOps configuration](/nyl/kubernetes-rendering/pipeline/#without-gitops-configuration).
- `--concurrency <N>` - Maximum number of resources applied or pruned at the same time (default: 8; `1` applies serially)

## Apply order and progress

Nyl applies resources in phases, whatever order the manifest lists them in:

1. CustomResourceDefinitions.
2. Namespaces.
3. Everything else in Argo CD's order: cluster policy (NetworkPolicies,
   ResourceQuotas, LimitRanges, PodDisruptionBudgets), ServiceAccounts, Secrets and
   ConfigMaps, storage, RBAC, Services, workloads, Ingresses, and APIServices,
   followed by every other kind, including custom resources.

A release can therefore define a CRD and custom resources of it, or a Namespace and
objects in it, and apply in one run. Nyl orders by kind only: the
[Argo CD sync waves and phases](/nyl/kubernetes-rendering/pipeline/#argo-cd-sync-waves-and-phases)
a manifest carries order what Argo CD applies, and `nyl apply` does not read them.
Built-in kinds are matched by API group and kind, so a
custom kind named like a built-in one, such as a CNI's `NetworkPolicy`, is ordered
as a custom resource. Admission webhook configurations are applied last, so a
webhook is not registered before the resources it admits while its backend may
still be starting. As in Argo CD, the resources of one kind form a batch that is
applied concurrently, up to `--concurrency` at a time, and each batch finishes
before the next starts. Kinds therefore never race a kind they may depend on,
while resources of one kind do not wait for each other.

Before applying the first resource of an API group that a CustomResourceDefinition
or APIService registered earlier in the same apply, Nyl refreshes API discovery
until the new kinds are served, for up to 30 seconds. A CustomResourceDefinition's
kinds are served once it is Established, so the custom resources apply after the
CRD is Established; the CRDs apply first, so the wait is usually already over, and
nothing waits for a CRD the cluster already serves. A custom resource without a
namespace takes the destination namespace when its CRD, in the same manifest or
already in the cluster, declares the kind namespaced. A kind that no manifest CRD
defines and the cluster does not know, such as one an APIService in the same
manifest will serve, keeps no namespace: set `metadata.namespace` on it. When several documents describe the same object
(the same group, kind, namespace, and name, in any API version; cluster-scoped
objects ignore a namespace), only the last one is applied and recorded. A resource
that fails to apply does not stop the others; the release is recorded as failed
and the command exits non-zero.

Nyl records the new release revision before applying anything. Two applies that
record the same revision at the same time therefore fail before the second
changes the cluster, but Nyl does not otherwise lock a release: do not run applies
of one release in parallel. If an apply fails or is interrupted (Ctrl-C) before it
completes, its revision is recorded as failed with only the resources it applied;
press Ctrl-C again to exit without waiting for that. A release that renders no resources
is still recorded, and its previously deployed resources are pruned.

An apply always applies every resource, which repairs drift, but it records no
new revision when it changed nothing: the latest revision is `Deployed`, records
the same manifest, resource keys, owner, and publication destination, and the
apply pruned nothing and failed nothing. The reserved revision is released, the
latest revision stays current with its original timestamps (the `[release]`
history limit still applies to it), and the command reports
`Release: <name> revision <N> unchanged in namespace <namespace>; no new revision recorded`. This keeps no-op applies
from pushing useful revisions out of the history limit, which `helm upgrade`
does not: it creates a revision every time. [`release rollback`](/nyl/reference/cli/release/#rollback),
a failed or interrupted apply, and any apply that changes or prunes something
always record a revision.

Pruning deletes the resources a release no longer contains in the reverse of the
apply order, so dependents go before what they depend on: admission webhook
configurations before the backends they call, custom resources and objects of
aggregated APIs before the CustomResourceDefinitions and APIServices behind them,
APIServices before their backends, then Namespaces, and CustomResourceDefinitions
last, after every object that needs them. A purge deletes in the same order. As with apply, the
resources of one kind are deleted concurrently. Before deleting the next kind,
Nyl waits until the deleted resources are gone, so finalizers run while the
controllers, CRDs, and webhooks they need still exist. After two minutes of waiting
in total, resources still terminating are reported and pruning continues without
waiting. A resource whose recorded apiVersion is no longer served is deleted
through a version that is; one whose kind is gone entirely (for example, its CRD
was deleted) is already gone. A resource that fails to delete, including one
behind an aggregated API that is unavailable, stays recorded in the new revision,
so the next apply retries deleting it, and the command exits non-zero. Changing a
resource's apiVersion does not prune it.

Like an Argo CD sync, pruning reads each live resource's
`argocd.argoproj.io/sync-options`: `Prune=false` keeps the resource and stops
tracking it, and `Prune=confirm` keeps it, still tracked, until an apply with
`--confirm-prune`. Managed Namespaces carry `Prune=confirm` by default, so
dropping a namespace from a Release never deletes it and its contents
unconfirmed. See [Pruning and deletion](/nyl/kubernetes-rendering/pipeline/#pruning-and-deletion).

Each resource's outcome (`+` created, `~` updated, `=` unchanged, `✗` failed) is
printed as soon as it completes, in completion order. With `--handoff`, a `↳`
line follows each resource whose `nyl` fields were released. The summary is followed by
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

# Apply to a kube context in a project without GitOps configuration
nyl apply --context kind-kind manifest.yaml

# Keep only the ten most recent release revisions
nyl apply --target production --history-limit 10 manifest.yaml

# Hand the resources over to Argo CD before enabling its sync
nyl apply --target production --handoff manifest.yaml
```

### Output

The first apply creates the Release's resources, including its managed
Namespaces. The release namespace shows as updated because Nyl creates it first
to store the release state:

```console
$ nyl apply --target production applications/web/release.yaml
~ Namespace web
+ Namespace web-jobs
+ ConfigMap web-jobs/cleanup-schedule
+ ConfigMap web/web-config
+ Deployment web/web

Summary: 4 created, 1 updated, 0 unchanged
Timings: discovery (aggregated) 0.09s, validation 0.00s, apply 0.12s, release 0.01s

Release: web revision 1 deployed successfully to namespace web
```

After the Release drops its `web-jobs` namespace, apply prunes the ConfigMap
there but keeps the managed Namespace, which awaits confirmation:

```console
$ nyl apply --target production applications/web/release.yaml
= Namespace web
= ConfigMap web/web-config
~ Deployment web/web
! Kept Namespace web-jobs: its sync option Prune=confirm needs --confirm-prune to delete it

Pruning 1 resources...
  ✓ Deleted ConfigMap web-jobs/cleanup-schedule


Summary: 0 created, 1 updated, 2 unchanged
Timings: discovery (aggregated) 0.08s, validation 0.00s, apply 0.02s, release 0.02s

Release: web revision 2 deployed successfully to namespace web
$ nyl apply --target production --confirm-prune applications/web/release.yaml
= Namespace web
= ConfigMap web/web-config
= Deployment web/web

Pruning 1 resources...
  ✓ Deleted Namespace web-jobs


Summary: 0 created, 0 updated, 3 unchanged
Timings: discovery (aggregated) 0.08s, validation 0.00s, apply 0.02s, release 0.02s

Release: web revision 3 deployed successfully to namespace web
```

Applying the same Release again changes nothing, so the latest revision stays
current and no revision is recorded. The resource lines and summary are those of
any apply; the last line differs:

```console
$ nyl apply --target production applications/web/release.yaml
...
Release: web revision 3 unchanged in namespace web; no new revision recorded
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
- Release state is tracked in Kubernetes Secrets in the release namespace. Use [`nyl release`](/nyl/reference/cli/release/) to inspect history or [roll back](/nyl/reference/cli/release/#rollback) to a previous revision.
- An apply that changed nothing records no revision (see [Apply order and progress](#apply-order-and-progress)), so repeating `nyl apply` does not age out older revisions.
- Each release keeps its five most recent revisions unless `[release] history_limit` says otherwise. The oldest revisions beyond the limit are deleted after each successful apply; a failed apply deletes nothing. Deleted revisions can no longer be rolled back to.
- The destination namespace and `spec.additionalNamespaces` are applied as
  managed Namespace objects owned by the release, by the group's `namespace`
  policy, as in the published tree. Nyl also creates the release namespace
  before anything else when it is missing, because the release state lives
  there.
- `--no-release` disables release tracking entirely. In this mode, `nyl` cannot compute or prune resources removed from subsequent applies.
- Resources without `metadata.namespace` take the destination namespace of the
  Release's Application, or `--namespace` for a file without a Release. The kube
  context's default namespace is never used. See
  [Namespaces in diff and apply](/nyl/kubernetes-rendering/pipeline/#namespaces-in-diff-and-apply).

## Manifest validation

Use `--validate` to run project-configured validators. See
[Manifest validation](/nyl/kubernetes-rendering/validation/manifest-validation/) for automatic validation, captured
CRD schemas, inherited Cluster contracts, and offline schema vendoring.
