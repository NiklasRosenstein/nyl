---
title: 'get and update'
---

## Inspect project resources

`nyl get` reports GitOps control resources discovered in project source:

```bash
nyl get repositories
nyl get clusters
nyl get argocd-instances
nyl get targets
nyl get app-projects
nyl get application-groups
nyl get cluster primary
```

Plural resource names are canonical and singular aliases select the same kind.
An optional name filters the result. Output contains the resource name, source
file and document, and concise kind-specific coordinates. The command does not
render workloads, resolve remote sources, or contact Kubernetes.

## Update cluster capabilities

Refresh `Cluster.spec.kubernetes` from the live cluster:

```bash
nyl capture cluster primary
nyl capture cluster primary --context admin@primary
nyl capture cluster primary --check
```

The explicit context wins over `Cluster.spec.live.context`. The update changes
only the selected Cluster document and preserves the rest of a shared YAML
file. `--check` reports drift without writing.

## Update source locks

Resolve the revisions of remote ApplicationGroup sources and `fromGit` Release
input bindings, and update their full commit locks:

```bash
nyl update source-locks
nyl update source-locks workloads
nyl update source-locks --target production
nyl update source-locks --check
```

A group name selects that ApplicationGroup and `--target` selects one
DeploymentTarget's `fromGit` locks; without either, every lock is updated. The
filters choose which locks move, never where: each lock's destination depends
only on the lock, so a filtered update agrees with an unfiltered `--check`.
Every lock moves to the head of its revision. Each repository is fetched once
per run. Locks are edited in place by their
position, so write `fromGit` bindings in block style.

Mutable revisions must refresh successfully before a lock is reported current
or updated. Immutable commit rendering can use an existing local cache.
