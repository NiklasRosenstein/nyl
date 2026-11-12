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

- `--only-source-kind <KIND>` - Filter top-level resources by kind (e.g., `ConfigMap`, `Deployment`) or by apiVersion/kind (e.g., `apps/v1/Deployment`) before expansion. A render with this option fails with `NYL-KUBERNETES-CHECKSUM-PARTIAL` when a remaining Pod template sets [`k8s.nyl/checksum`](/nyl/kubernetes-rendering/pod-template-checksums/#where-it-runs), because a digest of part of the Release would differ from the one other commands write.
- `--only-kind <KIND,...>` - Filter final rendered manifests to only include specific kinds (post-render).
- `--exclude-kind <KIND,...>` - Filter final rendered manifests to exclude specific kinds (post-render, mutually exclusive with `--only-kind`).
- `--target <TARGET>` - DeploymentTarget whose Cluster supplies values, capabilities, destination identity, and the default kube context. Optional when exactly one target is configured; required when several are available. A project without DeploymentTargets uses `--context` instead.
- `--release-only` - Settle the Release alone instead of taking it from the compiled target; see [`render`](/nyl/reference/cli/render/#common-options).
- `--allow-secret-inputs` - Let the project secrets provider and `NYL_*` environment variables affect the applied manifests.
- `--max-depth <MAX_DEPTH>` - Maximum evaluation depth for recursive resource expansion (default: 10)
- `--track-parent` - Track parent resource information in annotations

`--input`, `--inputs`, `--application-group`, `--defaults-only`, `--lookup`, `--refresh-lookup`,
and the cache options work as for [`render`](/nyl/reference/cli/render/#common-options).

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

### Hook Options

- `--hook-timeout <SECONDS>` - How long each wait lasts: for a step's hooks to complete, for a deleted hook to be gone, and for the workloads of a step to become healthy before a later sync wave or phase (default: 600; `0` waits until interrupted, as Argo CD does). When a wait runs out, the apply fails and says what it was waiting for. See [Hooks and sync waves](#hooks-and-sync-waves).

## Apply order and progress

Nyl applies resources in phases, whatever order the manifest lists them in:

1. CustomResourceDefinitions.
2. Namespaces.
3. Everything else in Argo CD's order: cluster policy (NetworkPolicies,
   ResourceQuotas, LimitRanges, PodDisruptionBudgets), ServiceAccounts, Secrets and
   ConfigMaps, storage, RBAC, Services, workloads, Ingresses, and APIServices,
   followed by every other kind, including custom resources.

A release can therefore define a CRD and custom resources of it, or a Namespace and
objects in it, and apply in one run. That is the order within one sync wave of one
phase: the
[Argo CD sync waves and phases](/nyl/kubernetes-rendering/pipeline/#argo-cd-sync-waves-and-phases)
a manifest carries order what comes before and after, and `nyl apply` reads them
(see [Hooks and sync waves](#hooks-and-sync-waves)); a Release that declares none is
one wave, applied by kind alone.
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
nothing waits for a CRD the cluster already serves. Only the kinds a CRD declares
are waited for, and, like Argo CD, Nyl does not wait for the backend of a CRD's
conversion webhook. A legacy `v1beta1` CRD that lists no `versions` still declares
its scope, but names no kind to wait for, so nothing waits for it. When several documents describe the same object
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

## Hooks and sync waves

`nyl apply` syncs a Release the way Argo CD syncs the same manifests, so a Release
behaves the same applied by hand, by [`release rollback`](/nyl/reference/cli/release/#rollback),
or by the [direct reconciler](/nyl/kubernetes-gitops/workflow/direct-reconciler/#hooks),
as it does under Argo CD. A resource hook is not an ordinary resource: it runs, and
the apply waits for it.

**Which documents are hooks.** `argocd.argoproj.io/hook` is a comma-separated list of
`PreSync`, `Sync`, `PostSync`, `SyncFail`, and `Skip`; a hook runs once in each phase
it names. Without a recognized Argo CD hook type, Helm's `helm.sh/hook` decides, as in
Argo CD:

| Annotation | What `nyl apply` does |
| --- | --- |
| `pre-install`, `pre-upgrade` | Runs as a `PreSync` hook (once, if both are named) |
| `post-install`, `post-upgrade` | Runs as a `PostSync` hook |
| `crd-install` | Applies the document as an ordinary resource |
| `pre-delete`, `post-delete`, `pre-rollback`, `post-rollback`, `test` | Leaves it out, with the warning below |
| `argocd.argoproj.io/hook: Skip` | Never applies it |
| `argocd.argoproj.io/hook: PreDelete` or `PostDelete` | Leaves it out, with the warning below |

An Argo CD hook annotation wins: once `argocd.argoproj.io/hook` names a phase, the
Helm annotation is not read. `helm.sh/hook-delete-policy` is read next to
`argocd.argoproj.io/hook-delete-policy`.

**Order.** A sync runs `PreSync`, then `Sync`, then `PostSync`. Within a phase it
runs by sync wave (`argocd.argoproj.io/sync-wave`, else `helm.sh/hook-weight`, else
0; lowest first), then by kind (the order above), then by name. The `Sync` phase holds
the ordinary resources and the `Sync` hooks, so sync waves order ordinary resources
too. A Namespace, and a CustomResourceDefinition, applies in the phase and wave of
the first document that needs it when that comes earlier: a document in the
Namespace, or an object of the kind the CRD defines, a hook included. A custom
resource of sync wave `-1` therefore gets its CRD and its Namespace first, though
both declare no wave, as in Argo CD. Each step (a phase and a wave) must finish
before the next starts:

- A hook has finished when a Job has completed, a Pod has succeeded, or an Argo
  Workflow has succeeded; a workload hook when it is healthy; a hook of any other
  kind once it exists. A hook that is deleted before it finishes has failed.
- Before a step is followed by another, the workloads it applied (Deployments,
  StatefulSets, DaemonSets, ReplicaSets, Jobs, Pods, Argo Workflows) must be
  healthy, as for `kubectl rollout status`. That is why `PostSync` hooks start only
  after the `Sync` phase is healthy. In a sync of more than one step the last step
  waits for them too when it holds a hook; the last step of ordinary resources
  alone is done once they are applied, and a Release of one step waits for nothing.
  Kinds without a health check count as healthy once applied. A workload that
  cannot be read is waited for until the wait runs out; one that is gone is not.
- A Release without hooks or sync waves is one step: nothing waits for a rollout.

**Failure.** A failed resource, a failed hook, or a wait that ran out fails the sync
after the step it happened in; no later step starts. The `SyncFail` hooks then run,
and the revision is recorded `Failed`. `PostSync` hooks run after pruning, so the
cluster is as the Release leaves it; a failed `PostSync` hook fails the revision,
though the resources were applied and pruned.

**Delete policies.** A hook with no policy is deleted before it is created again
(`BeforeHookCreation`): Nyl deletes an existing object in the foreground and waits
until it is gone, then creates it. A hook that is not there is not an error. With
`HookSucceeded` the object is deleted when the whole sync succeeded; with
`HookFailed` it is deleted when the sync failed. A hook object that already exists
and is not deleted by a policy is applied over, so a finished Job does not run again
until its policy deletes it.

**Hooks are not owned.** The revision's manifest holds the hooks, so `release
rollback` runs them again, but the revision does not track them: `apply` never prunes
a hook, and [`detect-drift`](/nyl/reference/cli/detect-drift/) and the health reading
never compare one. A hook that an earlier Nyl tracked as an ordinary resource is no
longer pruned once it is a hook. `release delete --purge` and a teardown do delete
the hooks the latest revision's manifest holds, with the rest of the Release: what a
hook left behind (a finished Job, a hook ServiceAccount or Role) would otherwise
outlive it.

**The hook finalizer.** Nyl creates a hook with Argo CD's
`argocd.argoproj.io/hook-finalizer`, and removes it once it has seen how the hook
ended, or when the hook is deleted or purged, so a Job with a small
`ttlSecondsAfterFinished` is still there to be read when it finishes. If Nyl is
interrupted while a hook runs, it removes the finalizer on the way out; if it cannot
(the cluster is unreachable), the next sync or purge of that hook removes it first.

**When hooks run.** Every apply runs every hook, as every Argo CD sync does,
including an apply that changed nothing and so records no new revision (see above):
the apply itself always runs, and the revision policy decides only whether a revision
is written. `release rollback` runs the hooks of the revision it applies. Make hooks
safe to run twice.

**Hooks left out.** Hooks that name no sync phase are not applied, as Argo CD does
not sync them. Helm's `test` hook and the rollback hooks (`pre-rollback`,
`post-rollback`) are left out silently, as Argo CD leaves them out: every chart made
by `helm create` declares a `test` hook, and it changes nothing here. [`nyl
diff`](/nyl/reference/cli/diff/#hooks) lists them, for information. Hooks Argo CD runs
when it removes an Application (`PreDelete`, `PostDelete`, `pre-delete`,
`post-delete`) are never run by Nyl when it removes a Release, and that matters:
`nyl apply` warns once per Release with `NYL-KUBERNETES-HOOK-IGNORED`, naming them,
and `release delete --purge` warns too, before it goes ahead. To act on the warning,
do what the hook does yourself before the Release is removed, drop the hook from the
Release, or mark it `argocd.argoproj.io/hook: Skip`, which leaves it out of Argo CD's
sync as well and ends the warning.

**Where Nyl differs from Argo CD.**

- Nyl does not wait two seconds between sync waves: it reads the workloads'
  `observedGeneration`.
- Argo CD reads the health of more kinds than Nyl does between waves, and runs custom
  Lua health checks. Nyl reads Deployments, StatefulSets, DaemonSets, ReplicaSets,
  Jobs, Pods, and Argo Workflows, and counts any other kind as healthy once applied:
  a Service of type `LoadBalancer`, an Ingress, a PersistentVolumeClaim, an
  APIService, and a HorizontalPodAutoscaler are not waited for.
- Nyl bounds each wait with `--hook-timeout` (ten minutes by default); Argo CD waits
  until the sync is terminated. A hook still running when the apply is interrupted
  (Ctrl-C) keeps running; Nyl removes its finalizer, and the hook is deleted by its
  policy at the next sync.
- A hook needs a `metadata.name`; `generateName` is not supported.
- `nyl apply` creates the Release's own destination namespace before the first step,
  so a hook of the first step can run in it though the Release declares no Namespace
  object. A Namespace the Release declares is applied with its first dependent, as
  above. Whether the same hook finds its namespace under Argo CD depends on how the
  namespace is created there.

## Which kinds are namespaced

A resource that sets no namespace takes the destination namespace when its kind is
namespaced. One resolver decides that scope, in one fixed order, for `apply`,
[`diff`](/nyl/reference/cli/diff/), `release rollback`, and the direct reconciler, and
for the Application and AppProject Nyl derives for a target:

1. The CustomResourceDefinitions of the Release being applied (the manifest's CRDs).
   A CRD names its kind's scope, and counts when it serves a version: a CRD that
   lists versions but serves none declares nothing. Where two CRD documents declare
   one kind, the later one wins. The direct reconciler and the tree also know the CRDs
   of the other Releases of the target.
2. With a DeploymentTarget (`--target`, or the target the project selects), the target
   Cluster's recorded Kubernetes capabilities, which include the CRDs vendored by
   [`nyl capture cluster --crds`](/nyl/kubernetes-gitops/resource-guides/cluster/).
3. The built-in Kubernetes kinds, whose scope is fixed.
4. The live cluster, only for a kind nothing above knows, and only for `nyl apply`,
   `diff`, and `release rollback`: the tree and AppProject derivation never ask a
   cluster. Without GitOps configuration, `--context` selects the cluster and there is
   no step 2.

The result therefore follows what is declared, and a custom resource whose CRD is
new in the manifest, or vendored but not yet installed, gets its namespace before
anything is applied. A stale vendored snapshot takes precedence over the live cluster,
so a kind whose scope changed in the cluster keeps the vendored scope until
`nyl capture cluster <name> --crds` refreshes the snapshot. When a scope the Cluster
records differs from what the connected cluster reports, `apply`, `diff`, and
`release rollback` still use the recorded scope and warn once per kind with
`NYL-KUBERNETES-SCOPE-MISMATCH`; its fix is to re-run
`nyl capture cluster <name> --crds` and commit the result. The direct reconciler
reports the same warning in its run. A kind that none of these
knows, such as one an APIService in the same manifest will serve, keeps no namespace:
set `metadata.namespace` on it.

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
