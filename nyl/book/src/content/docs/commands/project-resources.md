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

Move every Git lock to the head of its revision: the pinned commits of remote
ApplicationGroup sources and of `fromGit` Release input bindings:

```bash
nyl update source-locks
nyl update source-locks workloads
nyl update source-locks --target production
nyl update source-locks --check
```

An ApplicationGroup name or `--target <name>` selects which locks move, never
where they move. `--check` reports stale locks without modifying project
source. See the [Rendered GitOps command reference](/nyl/commands/gitops/) for
details.
