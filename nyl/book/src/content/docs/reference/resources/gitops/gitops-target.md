---
title: 'GitOpsTarget'
---

`GitOpsTarget` defines one independently rendered and published deployment
slice. It binds one concrete Cluster to deployment values and Git publication
coordinates.

## Example

```yaml
apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: GitOpsTarget
metadata:
  name: production
  labels:
    environment: production
spec:
  clusterRef:
    name: primary
  values:
    environment: production
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: production
  projects:
    - workloads
```

## Fields

| Field | Required | Description |
| --- | --- | --- |
| `metadata.name` | Yes | Target identity used by `--target`. |
| `metadata.labels` | No | Labels matched by ApplicationGroup target selectors. |
| `spec.clusterRef.name` | Yes | Local [`Cluster`](/nyl/reference/resources/gitops/cluster/) identity. |
| `spec.values` | No | Deployment values recursively overlaid on Cluster values. |
| `spec.publication.repositoryRef.name` | Conditional | Local [`GitRepository`](/nyl/reference/resources/gitops/git-repository/) identity. |
| `spec.publication.repository` | Conditional | Inline `repoURL` and optional `publishURL`. |
| `spec.publication.revision` | Yes | Git revision used for publication and generated Argo CD sources. |
| `spec.publication.pathPrefix` | No | Normalized relative path containing this target's tree. Defaults to the repository root. |
| `spec.projects` | No | Local AppProjectDefinition identities allowed for this target. An empty list permits all projects. |

Exactly one of `publication.repositoryRef` and `publication.repository` is
required. The path prefix cannot be absolute, contain traversal, or use
backslashes.

## Selection behavior

An ApplicationGroup applies when it is enabled, its project is permitted by
`spec.projects`, and its `targetSelector.matchLabels` matches the target's
metadata labels. An empty `projects` list does not restrict groups.

The target's Cluster supplies the generated Argo CD destination. Cluster and
target values merge recursively, with target values winning at conflicting
leaves.

## Publication models

Targets may share a revision when their path prefixes are disjoint. They may
also use separate revisions or repositories. Nyl rejects overlapping prefixes
on the same repository and normalized branch revision, including overlap
through read or write URL aliases.

See [Targets and cluster variation](/nyl/deployment-workflows/rendered-manifests/targets-and-clusters/)
for common deployment topologies.
