---
title: 'Targets and Cluster Variation'
---

A [`Cluster`](/nyl/reference/resources/gitops/cluster/) models one concrete
Kubernetes destination and its render-time capabilities. A
[`GitOpsTarget`](/nyl/reference/resources/gitops/gitops-target/) binds that
Cluster to deployment intent and publication coordinates.

## Operational models

The same model supports:

- one target with one publication revision;
- several targets on one revision under disjoint path prefixes;
- a revision per target; and
- targets published to different repositories.

Multiple targets can reference one Cluster when they need different values,
application selections, or publication cadences. A deployment slice for a
different concrete cluster uses another Cluster and GitOpsTarget. Publication
prefixes may overlap only when the repository or revision differs.

## Value overlays

Nyl merges Cluster values with target values recursively. Target values win at
each conflicting leaf:

```text
Cluster.spec.values < GitOpsTarget.spec.values
```

Cluster values describe facts such as region, architecture, storage class, and
ingress implementation. Target values describe deployment intent such as the
environment or rollout configuration.

The effective target and a sanitized Cluster are available to templates as
`target` and `cluster`; `Cluster.spec.live` is omitted. Merged values remain
available as `values`:

```yaml
data:
  environment: '{{ target.metadata.labels.environment }}'
  region: '{{ values.region }}'
  clusterName: '{{ cluster.metadata.name }}'
```

## Conditional applications and resources

`ApplicationGroup.spec.targetSelector.matchLabels` can omit a whole group for
a target. Nyl templating can omit individual workload resources, while the
Release document remains structurally present for source discovery.
Target-dependent structural templating in ApplicationGroup and
AppProjectDefinition specs can vary policy and project content while their
discovery envelopes remain static.

Each target renders with its Cluster's committed Kubernetes version and API
versions. This permits target-specific Helm capability checks without granting
CI cluster access.

## Maintaining cluster capabilities

Inspect and refresh committed capabilities from a live cluster with:

```bash
nyl cluster list
nyl cluster update primary
nyl cluster update primary --check
```

`cluster update` changes only `spec.kubernetes`, sorting and deduplicating API
versions. `--check` reports drift without writing. The explicit `--context`
wins over `Cluster.spec.live.context`; without either, Nyl uses the current
kubeconfig context.

Review and commit capability changes like any other platform configuration
change because they can alter rendered output. See the
[`cluster` command reference](/nyl/commands/cluster/) for live-context checks.

## Next steps

- [Cluster reference](/nyl/reference/resources/gitops/cluster/)
- [GitOpsTarget reference](/nyl/reference/resources/gitops/gitops-target/)
- [Rendering, diffing, and publishing](/nyl/deployment-workflows/rendered-manifests/rendering-and-publishing/)
