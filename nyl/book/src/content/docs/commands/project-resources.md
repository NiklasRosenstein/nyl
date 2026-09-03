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
nyl update cluster primary
nyl update cluster primary --context admin@primary
nyl update cluster primary --check
```

The explicit context wins over `Cluster.spec.live.context`. The update changes
only the selected Cluster document and preserves the rest of a shared YAML
file. `--check` reports drift without writing.

## Update source locks

Resolve mutable remote ApplicationGroup revisions and update their pinned
commits:

```bash
nyl update source-locks
nyl update source-locks workloads
nyl update source-locks --check
```

`--check` reports stale locks without modifying project source.
