---
title: 'argocd'
---

List and purge the live resources of an Argo CD Application from the
inventory it records, without generating manifests.

## Synopsis

```bash
nyl argocd resources <APPLICATION> [--target <TARGET> | --context <CONTEXT>] [OPTIONS]
nyl argocd purge <APPLICATION> [--target <TARGET> | --context <CONTEXT>] [OPTIONS]
```

## Description

Argo CD prunes and cascade-deletes an Application's resources only after it
has generated the Application's manifests. When the source path is deleted
from Git first, the Application fails with `InvalidSpecError: Unable to
generate manifests ... app path does not exist`, and deleting it cascades
nothing or hangs on its finalizer.

The Application still lists every resource it tracks in `status.resources`.
`nyl argocd resources` prints that list. `nyl argocd purge` deletes those
resources on the Application's destination cluster, keeping what an Argo CD
Application deletion keeps, and with `--delete-application` then removes the
Application. Neither command reads Git or renders anything.

## Finding the Application

With GitOps configuration, the DeploymentTarget decides both clusters, as for
[`diff` and `apply`](/nyl/kubernetes-rendering/pipeline/#without-gitops-configuration):

- The Application is read from the target's ArgoCDInstance: its Cluster and
  namespace, or the target's own Cluster and `argocd` when no instance is
  declared. `--context` replaces that Cluster's context.
- Resources are deleted on the target's Cluster, through
  `--destination-context` or `Cluster.spec.live.context`. Nyl checks both
  contexts against their Clusters' servers.
- The Application's `spec.destination` must be the target's Cluster
  (`NYL-ARGOCD-DESTINATION-MISMATCH`).

Without GitOps configuration, `--context` names the cluster Argo CD runs in and
`--argocd-namespace` the Application's namespace, `argocd` by default:

- An Application deploying to the cluster Argo CD runs in
  (`https://kubernetes.default.svc` or `in-cluster`) is purged through the same
  context.
- Any other destination needs `--destination-context`
  (`NYL-ARGOCD-DESTINATION-CONTEXT`). When the Application names its
  destination by server, that context must reach it
  (`NYL-ARGOCD-DESTINATION-MISMATCH`).

## resources

```bash
nyl argocd resources web --context kind-kind
nyl argocd resources web --target production -o json
```

Prints the API version, kind, namespace, name, and recorded sync status of each
resource in `status.resources`. It reads only the Application. `-o json` and
`-o yaml` print the Application's name, namespace, destination, resources, and
finalizers.

## purge

```bash
# Preview: classify each resource by its live object, deleting nothing
nyl argocd purge web --context kind-kind --dry-run

# Delete the resources, then the Application
nyl argocd purge web --context kind-kind --yes --delete-application

# An Application deploying to another cluster than Argo CD runs in
nyl argocd purge web --context argocd-admin --destination-context prod-admin
```

For each tracked resource, `purge` reads the live object and decides:

| Result | When |
| --- | --- |
| `would-delete` / `deleted` | The object carries the Application's tracking and no sync option keeps it. |
| `already-gone` | The object, or its whole kind, no longer exists. |
| `retained` | Its sync option is `Delete=false`, which an Application deletion keeps too. |
| `awaiting-confirmation` | Its sync option is `Delete=confirm`, such as a managed Namespace, and `--confirm-delete` was not passed. |
| `not-tracked` | Its `argocd.argoproj.io/tracking-id` annotation, or without one its `app.kubernetes.io/instance` label, names another Application or none. |
| `failed` | Reading or deleting it failed. |

The tracking check accepts `<application>` and, for Applications outside the
control-plane namespace, `<namespace>_<application>`. It keeps a purge from
deleting an object another owner adopted after Argo CD recorded it.

Deletion follows [`nyl apply` pruning](/nyl/reference/cli/apply/#apply-order-and-progress):
resources are deleted in reverse apply order, dependents first, and each kind
waits for the previous one to terminate. Deletion uses foreground propagation
unless `--propagation` says otherwise, so controllers clean up the objects they
own first. `purge` asks for confirmation unless `--yes` is passed.

With `--delete-application`, once every resource is `deleted`,
`already-gone`, or `retained`, Nyl removes the Application's finalizers and
deletes it, so Argo CD does not retry a cascade it cannot compute. When
anything else remains, the Application is kept
(`NYL-ARGOCD-APPLICATION-KEPT`).

### Options

| Option | Meaning |
| --- | --- |
| `<APPLICATION>` | The Application's name. |
| `--target <TARGET>` | The DeploymentTarget whose ArgoCDInstance runs the Application. Optional when exactly one is configured. |
| `--context <CONTEXT>` | The Argo CD cluster's kube context: replaces the instance Cluster's context, or selects the cluster without GitOps configuration. |
| `--argocd-namespace <NAMESPACE>` | Without GitOps configuration, the Application's namespace. Default `argocd`. |
| `--destination-context <CONTEXT>` | (`purge`) The kube context of the cluster the Application deploys to. |
| `--dry-run` | (`purge`) Report what would happen without deleting anything. |
| `-y`, `--yes` | (`purge`) Skip the confirmation prompt. |
| `--confirm-delete` | (`purge`) Also delete resources whose sync option is `Delete=confirm`. |
| `--ignore-tracking` | (`purge`) Delete resources whatever tracking their live object carries. |
| `--propagation <foreground\|background\|orphan>` | (`purge`) How dependents of deleted resources are handled. Default `foreground`. |
| `--grace-period <SECONDS>` | (`purge`) The deletion grace period; by default each resource's own. |
| `--concurrency <N>` | (`purge`) The most resources deleted at a time. Default 8. |
| `--delete-application` | (`purge`) After a clean purge, remove the Application's finalizers and delete it. |
| `-o`, `--output <table\|json\|yaml>` | A table, or the typed report. Default `table`. |

### Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every resource was deleted, already gone, or retained, or a dry run. |
| 1 | An error, such as an unknown Application or a destination mismatch. |
| 2 | Resources await `--confirm-delete` or do not carry the Application's tracking. |
| 3 | A resource could not be read or deleted. |

### Codes

| Code | Fix |
| --- | --- |
| `NYL-ARGOCD-APPLICATION-UNKNOWN` | Check the Application name and its namespace (`--argocd-namespace`, or the target's ArgoCDInstance). |
| `NYL-ARGOCD-DESTINATION-MISMATCH` | Select the cluster the Application deploys to: another `--target`, or the right `--destination-context`. |
| `NYL-ARGOCD-DESTINATION-CONTEXT` | Pass `--destination-context` with the kube context of the Application's destination cluster. |
| `NYL-ARGOCD-NO-RESOURCES` | Nothing to purge. If resources remain, find them by the Application's tracking label or annotation. |
| `NYL-ARGOCD-DELETE-CONFIRM` | Pass `--confirm-delete` to delete resources marked `Delete=confirm`. |
| `NYL-ARGOCD-NOT-TRACKED` | Check that no other owner manages the resource, then pass `--ignore-tracking`. |
| `NYL-ARGOCD-DELETE-FAILED` | Fix the error it shows and run the purge again. |
| `NYL-ARGOCD-APPLICATION-KEPT` | Resolve the flagged resources, then run the purge again with `--delete-application`. |

## See Also

- [`release delete`](/nyl/reference/cli/release/#delete) - Purge the resources `nyl apply` recorded
- [Pruning and deletion](/nyl/kubernetes-rendering/pipeline/#pruning-and-deletion) - The sync options direct commands honour
