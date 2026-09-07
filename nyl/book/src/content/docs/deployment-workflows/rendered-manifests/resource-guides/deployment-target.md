---
title: 'Using DeploymentTarget'
---

See the [DeploymentTarget resource reference](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/) for the API, example, and field definitions.

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
