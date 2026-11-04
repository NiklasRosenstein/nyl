---
title: 'Targets and Cluster Variation'
---

A [`Cluster`](/nyl/reference/resources/k8s.nyl/v1/cluster/) models one concrete
Kubernetes destination and its render-time capabilities. A
[`DeploymentTarget`](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/) binds that
Cluster to deployment intent and publication coordinates.

## Operational models

The same model supports:

- one target with one publication revision;
- several targets on one revision under disjoint path prefixes;
- a revision per target; and
- targets published to different repositories.

Multiple targets can reference one Cluster when they need different values,
application selections, or publication cadences. A deployment slice for a
different concrete cluster uses another Cluster and DeploymentTarget. Publication
prefixes may overlap only when the repository or revision differs.

An ArgoCDInstance models the control plane separately. Several targets can
deploy to separate workload Clusters while their generated catalogs are owned
by the same Argo CD installation. Explicit instances require explicit
`DeploymentTarget.spec.argocdRef` bindings.

## Value overlays

Nyl merges file imports and inline values recursively, in this order from lowest
to highest precedence:

```text
Cluster.spec.valuesFrom < Cluster.spec.values < DeploymentTarget.spec.valuesFrom < DeploymentTarget.spec.values
```

Cluster values describe facts such as region, architecture, storage class, and
ingress implementation. Target values describe deployment intent such as the
environment or rollout configuration.

### File-backed values

Both resources accept `spec.valuesFrom`, a list of plain YAML mapping files.
Imports apply in declaration order, with later files winning; inline `values`
override imports within each resource. An optional `key` mounts the mapping under
one literal values key. Omitting it merges the mapping at the values root.

For example, with `nyl.toml` in `nyl/`, a target can read a shared file elsewhere
in the same repository:

```yaml
# nyl/config/targets/production.yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
spec:
  clusterRef: {name: primary}
  valuesFrom:
    - file: ../../../shared/platform.yaml
      key: platform
    - file: /shared/production.yaml
  values:
    platform:
      backup:
        retentionDays: 14
  publication:
    repository: {repoURL: 'https://github.com/example/deploy.git'}
    revision: deploy
```

```yaml
# shared/platform.yaml
labels:
  team: engineering
backup:
  retentionDays: 30
```

```yaml
# shared/production.yaml
platform:
  labels:
    environment: production
```

Templates read `values.platform.labels.team` as `engineering` and
`values.platform.backup.retentionDays` as `14`. The same file can also be
imported by a Cluster or read directly by another tool.

Objects merge recursively. Arrays, scalars, and `null` replace the earlier value
whole; `null` remains a value, rather than deleting the key. An empty array or
empty string replaces the earlier value. An empty object merges without removing
existing object keys, and replaces a non-object with an empty object. An omitted
`values` block or `values: {}` adds no overrides. `key` is a literal key, so
`key: platform.backup` creates one key containing a dot.

Unlike other [local paths](/nyl/reference/configuration/#local-paths), relative
`valuesFrom.file` paths resolve from the directory containing the resource's
source YAML file, independently of the working directory. A leading `/` resolves
from the Git worktree root. Paths must be normalized, may use leading `..`
segments, and must stay inside that worktree without traversing symbolic links
or accessing `.git` metadata. Files may live outside the directory containing
`nyl.toml`.

Imports follow Git visibility: tracked files and non-ignored untracked regular
files are eligible. Ignored untracked files, submodules, and the project's
`.nyl/cache` are excluded. There is no filename-extension restriction.

Each file must contain exactly one mapping document; an empty file, scalar,
array, `null` root, or multiple documents is an error. Nyl reads these as
ordinary configuration without templating or recursive imports. Quoted template
expressions remain literal strings. Loading imports needs no network or cluster
access, and does not grant access to secrets or process environment.

Imported files participate in render caching, ownership-index input digests, and
publication provenance. Commit them alongside the resource declarations:
`publish-tree` verifies the rendered target against the committed source before
attributing output to it. See [Publishing](/nyl/kubernetes-gitops/workflow/publish/).

The effective target and a sanitized Cluster are available to templates as
`target` and `cluster`; `Cluster.spec.live` is omitted. Merged values remain
available as `values`:

```yaml
data:
  environment: '{{ target.metadata.labels.environment }}'
  region: '{{ values.region }}'
  clusterName: '{{ cluster.metadata.name }}'
```

Local components can declare [value imports](/nyl/kubernetes-rendering/components/value-imports/)
to receive selected values as chart value defaults without templating them
into every invocation.

## Conditional applications and resources

`DeploymentTarget.spec.applicationGroupSelector.matchLabels` selects groups using
their static metadata labels. Nyl templating can omit individual workload resources, while the
Release document remains structurally present for source discovery.
Target-dependent structural templating in ApplicationGroup and
AppProjectDefinition specs can vary policy and project content while their
discovery envelopes remain static.

Each target renders with its Cluster's committed Kubernetes version and API
versions. This permits target-specific Helm capability checks without granting
CI cluster access.

## Maintaining cluster capabilities

`nyl get clusters` shows each Cluster's committed Kubernetes version and the
kube context `capture` uses:

```console
$ nyl get clusters
NAME     DESTINATION                       CONTEXT        VERSION  FILE
primary  https://primary.example.com:6443  admin@primary  1.32.2   config/clusters/primary.yaml#document-1
staging  https://staging.example.com:6443  -              1.31.4   config/clusters/staging.yaml#document-1
```

Refresh the committed capabilities from the live cluster, or check them for
drift:

```bash
nyl capture cluster primary
nyl capture cluster primary --check
```

`capture cluster` refreshes `spec.kubernetes`, sorting and deduplicating API
versions and recording which kinds are cluster-scoped. With CRD capture enabled
it also vendors the cluster's CustomResourceDefinitions and records only the
APIs they do not serve; rendering adds the CRD-served API versions back from the
snapshot. `--check` reports drift without writing. The explicit `--context`
wins over `Cluster.spec.live.context`; without either, Nyl uses the current
kubeconfig context.

Review and commit capability changes like any other platform configuration
change because they can alter rendered output. See the
[`get and update` command reference](/nyl/reference/cli/project-resources/) for live-context checks.

## Next steps

- [Cluster reference](/nyl/reference/resources/k8s.nyl/v1/cluster/)
- [DeploymentTarget reference](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/)
- [Render and validate](/nyl/kubernetes-gitops/workflow/render/)
