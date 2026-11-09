---
title: 'Apply Directly Without Argo CD'
description: 'Declare a DeploymentTarget that Nyl applies itself, what it publishes, how releases are fenced, and how it is read and torn down.'
---

A DeploymentTarget is delivered one of two ways, and says which in
`spec.reconciler`. With `reconciler: {argocd: …}`, Argo CD syncs the published
tree. With `reconciler: {direct: {}}`, Nyl applies the target's
Releases to its Cluster itself, with the mechanics of
[`nyl apply`](/nyl/reference/cli/apply/): server-side apply as the field manager
`nyl`, pruning by release state, and the Argo CD `Prune` and `Delete` sync
options. It still publishes the tree.

```yaml
apiVersion: k8s.nyl/v1
kind: Cluster
metadata: {name: kind-dev}
spec:
  destination: {server: https://127.0.0.1:6443}
  live: {context: kind-dev}        # the context a direct target is applied and read through
---
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {name: dev}
spec:
  clusterRef: {name: kind-dev}
  reconciler:
    direct: {}                     # or argocd: {instanceRef: {name: <instance>}}
  publication: {repositoryRef: {name: deploy}, revision: deploy, pathPrefix: targets/dev}
```

`reconciler` is required and names exactly one member, `argocd` or `direct`: a
target never has an implied reconciler. `reconciler.direct` is exclusive with any
`catalogApplication` setting, because a direct target generates no Application,
AppProject, or catalog, so a project of direct targets declares no ArgoCDInstance,
and a project with explicit instances sets `reconciler.argocd.instanceRef` on its
Argo CD targets alone. `ApplicationGroup` fields that describe Argo CD (`applicationNamespace`,
`projectRef`, `projectTemplate`, `syncPolicy`, and the like) are ignored for a
direct target, because one group may be selected by targets of both kinds. A
Namespace with a `Dedicated` shared owner is rejected, because it has no Release to
hold its release state: own it by a Release, or declare it `External`.

## What is published

`render-tree` and `publish-tree` write the Release directories and
`_nyl/index.json`, whose `reconciler: direct` member is part of the tree's
identity: a target never changes reconciler by a publication, because removing an
Argo CD target's catalog could let a self-managing catalog delete its workloads
(`NYL-TREE-RECONCILER-CHANGED`). They apply nothing. To move a target from Argo CD
to direct without stopping its workloads, delete its Applications with the
resources orphaned (`kubectl delete application --cascade=orphan`), release the
old unit with `nyl state forget`, and publish under the new declaration; the
forced server-side apply takes the objects over.

## Through a KubernetesPublication

A [`KubernetesPublication`](/nyl/orchestration/resource-guides/kubernetes-publication/#direct-targets)
unit places a target and does not know which reconciler runs it. For a direct
target, `nyl reconcile` publishes the tree and then applies each Release from the
bytes it pushed, so the cluster never runs what no commit holds. The unit needs the
Cluster's `live.context` and the kubeconfig it admits as `KUBECONFIG`; the context's
server must be the one the Cluster declares.

Releases apply one at a time in waves: Releases that render CustomResourceDefinitions
or APIServices come first. Each Release is prepared when its wave comes, so a
custom resource without a namespace takes its scope from a CRD of any Release of the
target or from the target Cluster's recorded capabilities (vendored CRDs included)
before the live cluster is asked; see
[which kinds are namespaced](/nyl/reference/cli/apply/#which-kinds-are-namespaced). A Release applied after
another that registered kinds waits, within the unit's deadline, until the cluster
serves the kinds that Release registered and this one uses; it does not wait for
a Release that failed. Inside a Release, CustomResourceDefinitions apply first, then
Namespaces, then the other kinds, as
[`nyl apply`](/nyl/reference/cli/apply/#apply-order-and-progress) applies them.
No Release starts once the unit's deadline has passed, and the run fails as a
retryable timeout. A Release never prunes an object another Release
of the target renders, so moving an object between Releases leaves it in place; if
the Release that now renders it has not applied yet, the old one keeps tracking it
until it does. A
prune or purge never deletes an object whose `Prune` or `Delete` sync option it
cannot read. A Release that left Git is purged after every Release applied, unless
no unit owns it, such as one an apply by hand took over: that stays, with the
warning `NYL-KUBERNETES-RELEASE-LEFT-BEHIND`. Which Releases left is read from the
publication branch back to the last receipt; a history deeper than the walk limit
that never reaches it (or with no receipt at all) fails with
`NYL-KUBERNETES-RELEASE-HISTORY-TOO-DEEP` instead of guessing.

## Hooks

A Release applied by Nyl handles its resource hooks (`argocd.argoproj.io/hook` and
`helm.sh/hook`) the way Argo CD syncs the same manifests, so a Release behaves the same
under either reconciler: `PreSync` hooks complete before anything else is applied,
the resources apply by sync wave, `PostSync` hooks run once the workloads are healthy
and the Release is pruned, `SyncFail` hooks run after a failure, and each hook's delete
policy decides when its object goes. The rules are those of
[`nyl apply`](/nyl/reference/cli/apply/#hooks-and-sync-waves), which share the
implementation with the unit.

- **When they run.** A hook runs on every execution of the unit that applies its
  Release, as it runs on every Argo CD sync: when the unit's key changed, and when
  drift repair executes it again, which is also when a self-healing Argo CD runs
  them. An execution that changed nothing in the release state records no revision
  and still runs the hooks. A hook that is gone does not make the unit drift, so it
  does not run again for that.
- **What the revision holds.** The recorded manifest holds the hooks, so
  `nyl release rollback` of a Release the unit applied runs them again; the tracked
  keys do not, so a hook is never pruned, never purged by a teardown, and never read
  as a workload by `nyl attest` or the health reading.
- **Failure.** A failed hook, or a wait that exceeds the unit's deadline, fails the
  unit as a retryable `tool-error` or `timeout`, naming the Release and the hook; Git
  stays ahead and the next execution applies again. The unit waits for hooks and
  workloads until its deadline (`timeout`), not for the ten minutes of
  `nyl apply --hook-timeout`.
- **Hooks left out.** Hooks with no sync phase (Helm's `test`, `pre-delete`,
  `post-delete`, and the rollback hooks, Argo CD's `PreDelete` and `PostDelete`) are
  not applied; each execution warns once per Release with
  `NYL-KUBERNETES-HOOK-IGNORED`. Nyl does not run delete hooks when a teardown
  removes a Release.

## Unchanged Releases

Every reconcile applies every Release, and runs its [hooks](#hooks) again, which
repairs anything changed in the
cluster by hand, except a Job that sets `ttlSecondsAfterFinished`, is gone from the
cluster, and has the document the Release's latest `Deployed` revision for this
publication holds: it ran and its TTL removed it, and applying it again would run it again. It stays tracked, and the Release
is judged unchanged without it. A Release that changed nothing does not record a new release
revision, so the history keeps its useful revisions instead of filling with
copies of one. A Release is unchanged when its latest revision is `Deployed`
and records the same manifest, resource keys, owner, and publication
destination (repository, branch, and prefix), and the apply pruned nothing and
failed nothing. The latest revision then stays the current one, with its
original timestamps, and `nyl release history` shows no new line.

A publication commit changes whenever any part of the target's tree changes, so
it is not compared. The kept revision records the older commit, which
`nyl attest` accepts as long as that commit is still on the publication branch
and its Release directory and recorded inputs equal the new publication's, the
same rule that matches an Argo CD revision. When they do not, for instance after
the branch was rewritten, the apply records a new revision with the new commit.
A `Failed` or interrupted latest revision always gets a new one, and so does a
Release that pruned something.

## Detecting drift

Nothing reconciles a direct target between runs, so a `kubectl edit`, a deleted
object, or another tool's change goes unnoticed until something looks.
[`nyl detect-drift`](/nyl/reference/cli/detect-drift/) of a `KubernetesPublication`
unit with a direct target compares the cluster with what the unit last applied, besides
comparing the publication branch with the tree as it does for Argo CD:

- **What is compared.** For each Release, the manifest of its newest `Deployed`
  revision applied for this unit's publication (repository, branch, and prefix),
  which is what the unit applied, never the declaration as it reads now. A newer
  revision of another publication, or one applied by hand, is not used as the desired
  state. A Release with no revision of this publication is drift, so a Release only
  ever applied by hand, or by a Nyl that recorded no publication, reports drift until
  the unit reconciles.
- **How.** Each object is read, then compared by a server-side apply dry run as the
  field manager `nyl`, the comparison `nyl diff` makes. Only fields the manifest
  declares count: a field only another manager sets, such as the replicas an
  autoscaler owns when the manifest leaves them out, is not drift. An object the
  manifest declares that changed, or that is gone, is drift. An object that changes
  between the read and the dry run (an autoscaler scaling it) is compared again once.
- **What is not drift.** A Job whose `ttlSecondsAfterFinished` let the cluster remove
  it after it finished. A [hook](#hooks), whether it is gone, changed, or finished: it
  runs when a sync runs it and its delete policy removes it, so it is never compared.
  An object two Releases render is compared once, against the
  Release that applies last (the order of the apply), which is what the cluster holds.
- **What is not compared.** Objects a Release keeps tracked without a document in its
  manifest: because their `Prune=confirm` option awaits confirmation, because their
  prune failed, or because another Release has yet to apply them. Objects carrying Nyl's
  labels that no current revision tracks (orphans) are not reported.
- **What it says.** `drifted`, naming each Release and the first eight changed or
  missing objects, then how many more; run `nyl diff --target <target>` on the
  Release's file to see each change in full. A kind the cluster no longer serves at the
  manifest's version, while it serves another, is drift worded "its apiVersion is no
  longer served"; a kind that is gone altogether is missing.
- **What it cannot tell.** An unreachable cluster, credentials that may not read the
  objects or run the dry run, an object the API rejects, an aggregated API that is
  down, or an unfinished comparison at the unit's deadline is `unknown`, never `clean`.
  Drift found anyway is reported as drift, with what stayed uncompared. The check reads
  Secrets of the Release namespaces for the release state, the objects the Releases
  apply, and needs `patch` for the dry runs, and, to tell a removed kind from an
  unavailable one, `get` on customresourcedefinitions and `list` on apiservices. An admission webhook whose `sideEffects`
  are `Unknown` or `Some` refuses dry runs, so a unit with such a webhook on one of its
  objects stays `unknown`.

The check does one read and one dry run per object, eight at a time, and stops a tenth
of the unit's `timeout` (at most ten seconds) before it so it can still report what it
found. Like a reconcile it uses only the kubeconfig the unit admits and the Cluster's
declared context. The cluster is compared even when the publication branch already
drifted: the two are independent, and the summary names both.

A drifted unit is repaired like any other: the next `nyl reconcile` under the default
`driftRepair: converge` executes it again, which applies every Release again (see
[Unchanged Releases](#unchanged-releases)). The apply puts back what was changed or
deleted and keeps the recorded revision, so a repair that changed nothing in the release
state records no revision. A Job its TTL removed is not applied again, here or in any
other reconcile, while its document is the one the Release's latest `Deployed` revision
for this publication holds (a later `Failed` revision does not matter): it would run a
second time. A Job whose document changed is applied.

## Fencing and manual changes

The release state in the cluster records the owning unit and the publication it was
applied for. A second unit cannot apply a Release another unit owns
(`NYL-KUBERNETES-RELEASE-FENCED`), and neither can a manual command: `nyl apply`,
`nyl release rollback`, and `nyl release delete` refuse it with
`NYL-RELEASE-OWNED-BY-UNIT`. `--take-over` records no owner and goes ahead; the
owning unit's next reconcile adopts the Release again and reverts the change.
`nyl diff` only reads and is never refused.

## Teardown

Teardown deletes in two phases behind `teardownWait`. Phase 1 publishes the tree
without its Releases and deletes their resources, honoring each object's `Delete`
sync option, but keeps the release state and the Namespaces that hold it. Phase 2,
once the wait ends, deletes the state and then those Namespaces by their
`deletePolicy`: `Automatic` removes them, `Confirm` and `Retain` leave them as a
remnant that `nyl plan` warns about. Phase 1 keeps every Namespace that holds the
state of any Release it removes, whichever Release owns the Namespace. Phase 2
deletes a Namespace only after the state in it is gone, and keeps the state of the
Release that tracked it until the Namespace is deleted, so a failed deletion is
retried; a Namespace that still holds the state of a Release that is not removed
stays, with a warning. A Release
that another unit or an apply by hand took over is left with its objects and its
Namespaces, and the teardown reports it with `NYL-KUBERNETES-RELEASE-LEFT-BEHIND`.
If the target's declaration changed reconciler since the unit published,
the teardown refuses with `NYL-KUBERNETES-RECONCILER-CHANGED`; restore the
declaration so it can run. Only `deletePolicy` matters to a direct
teardown; `prunePolicy` decides whether the next apply deletes a Namespace a
Release stopped declaring (`Confirm`, the default, keeps it with a warning). A
group shared with an Argo CD target declares both, as
`namespace: {prunePolicy: Automatic, deletePolicy: Automatic}`, because Argo CD
holds a teardown for either being `Confirm`.
