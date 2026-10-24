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

Each kind prints one table row per resource:

```console
$ nyl get clusters
NAME     DESTINATION                       CONTEXT        VERSION  FILE
primary  https://primary.example.com:6443  admin@primary  1.32.2   config/clusters/primary.yaml#document-1
staging  https://staging.example.com:6443  -              1.31.4   config/clusters/staging.yaml#document-1
$ nyl get targets
NAME        CLUSTER  PUBLICATION      PATH-PREFIX  FILE
production  primary  platform@deploy  production   config/targets/production.yaml#document-1
staging     staging  platform@deploy  staging      config/targets/staging.yaml#document-1
$ nyl get application-groups
NAME        PROJECT                 SOURCE                   FILE
monitoring  monitoring (generated)  applications/monitoring  applications/monitoring/group.yaml#document-1
web         web (generated)         applications/web         applications/web/group.yaml#document-1
```

`CONTEXT` is the Cluster's `spec.live.context`, `PUBLICATION` the target's
repository and revision, and `PROJECT` the ApplicationGroup's AppProject, marked
`(generated)` when Nyl derives it.

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
