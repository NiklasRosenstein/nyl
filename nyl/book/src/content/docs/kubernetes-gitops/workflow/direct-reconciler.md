---
title: 'Apply Directly Without Argo CD'
description: 'Declare a DeploymentTarget that Nyl applies itself, what it publishes, how releases are fenced, and how it is read and torn down.'
---

A DeploymentTarget is delivered one of two ways. By default Argo CD syncs the
published tree. With `reconciler: {direct: {}}`, Nyl applies the target's
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
    direct: {}                     # absent: Argo CD
  publication: {repositoryRef: {name: deploy}, revision: deploy, pathPrefix: targets/dev}
```

`reconciler` and `argocdRef` are mutually exclusive, and so are `reconciler.direct`
and any `catalogApplication` setting: a direct target generates no Application,
AppProject, or catalog, so a project of direct targets declares no ArgoCDInstance,
and a project with explicit instances sets `argocdRef` on its Argo CD targets
alone. `ApplicationGroup` fields that describe Argo CD (`applicationNamespace`,
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
first), within the unit's deadline: no Release starts once it has passed, and the
run fails as a retryable timeout. A Release never prunes an object another Release
of the target renders, so moving an object between Releases leaves it in place. A
prune or purge never deletes an object whose `Prune` or `Delete` sync option it
cannot read. A Release that left Git is purged after every Release applied, unless
no unit owns it, such as one an apply by hand took over: that stays, with the
warning `NYL-KUBERNETES-RELEASE-LEFT-BEHIND`.

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
state of any Release it removes, whichever Release owns the Namespace. A Release
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
