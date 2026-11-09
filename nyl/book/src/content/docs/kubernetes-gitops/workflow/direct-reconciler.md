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

Releases apply one at a time in waves (Releases that render CustomResourceDefinitions
first, each prepared when its wave comes so a custom resource without a namespace
resolves after its CRD applied, unless the CRD is in the same Release, which
declares the scope itself). Inside a Release, CustomResourceDefinitions apply first, then Namespaces, then
the other kinds, as [`nyl apply`](/nyl/reference/cli/apply/#apply-order-and-progress) applies
them, within the unit's deadline: no Release starts once it has passed, and the
run fails as a retryable timeout. A Release never prunes an object another Release
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

## Unchanged Releases

Every reconcile applies every Release, which repairs anything changed in the
cluster by hand. A Release that changed nothing does not record a new release
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
