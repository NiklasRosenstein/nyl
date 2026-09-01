---
title: 'DeploymentTarget'
---

`DeploymentTarget` defines one independently rendered and published deployment
slice. It binds one concrete Cluster to deployment values and Git publication
coordinates.

## Example

```yaml
apiVersion: gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
spec:
  clusterRef:
    name: primary
  argocdRef:
    name: central
  applicationGroupSelector:
    matchLabels:
      environment: production
  values:
    environment: production
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: production
  catalogApplication:
    name: production-catalog
```

## Fields

| Field | Required | Description |
| --- | --- | --- |
| `metadata.name` | Yes | Target identity used by `--target`. |
| `spec.clusterRef.name` | No | Local [`Cluster`](/nyl/reference/resources/gitops/cluster/) identity. Defaults to `metadata.name`. |
| `spec.argocdRef.name` | Conditional | [`ArgoCDInstance`](/nyl/reference/resources/gitops/argocd-instance/) that manages this target. Required when any explicit instance exists. |
| `spec.applicationGroupSelector.matchLabels` | No | Equal-match selector applied to static ApplicationGroup metadata labels. An empty selector matches all groups. |
| `spec.values` | No | Deployment values recursively overlaid on Cluster values. |
| `spec.publication.repositoryRef.name` | Conditional | Local [`GitRepository`](/nyl/reference/resources/gitops/git-repository/) identity. |
| `spec.publication.repository` | Conditional | Inline `repoURL` and optional `publishURL`. |
| `spec.publication.revision` | Yes | Git revision used for publication and generated Argo CD sources. |
| `spec.publication.pathPrefix` | No | Normalized relative path containing this target's tree. Defaults to `metadata.name`; set it explicitly to `""` for the repository root. |
| `spec.catalogApplication` | No | Parent catalog Application enablement and per-field overrides. |

Exactly one of `publication.repositoryRef` and `publication.repository` is
required. The path prefix cannot be absolute, contain traversal, or use
backslashes.

## Selection behavior

The target selects ApplicationGroups whose static `metadata.labels` satisfy
`spec.applicationGroupSelector.matchLabels`. Selection happens before target
templating; `ApplicationGroup.spec.enabled` can then omit a selected group for
the effective target.

The target's Cluster supplies the generated Argo CD destination. Cluster and
target values merge recursively, with target values winning at conflicting
leaves.

By default, Nyl emits `<target>-catalog` beneath
`_nyl/catalog/applications/<argocd-namespace>/`. It recursively syncs the
target's `_nyl/catalog` directory. Set `catalogApplication.enabled: false` only
when another trusted mechanism applies generated catalog resources.

## Publication models

Targets may share a revision when their path prefixes are disjoint. They may
also use separate revisions or repositories. Nyl rejects overlapping prefixes
on the same repository and normalized branch revision, including overlap
through read or write URL aliases.

See [Targets and cluster variation](/nyl/deployment-workflows/rendered-manifests/targets-and-clusters/)
for common deployment topologies.
