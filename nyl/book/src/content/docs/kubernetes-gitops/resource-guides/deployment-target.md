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

`spec.valuesFrom` loads plain YAML mapping files over Cluster values and before
inline target values. See
[file-backed values](/nyl/kubernetes-gitops/targets-and-clusters/#file-backed-values)
for paths, merge order, and examples.

By default, Nyl emits `<target>-catalog` beneath
`_nyl/catalog/application.argoproj.io/<argocd-namespace>/`. It recursively syncs the
target's `_nyl/catalog` directory. Set `catalogApplication.enabled: false` only
when another trusted mechanism applies generated catalog resources.

`catalogApplication.shared: {name: previews}` replaces the target's catalog
Application with one Application that many targets share, such as every preview
on a branch: it sources the branch root with an include pattern for its own
manifest and every target's catalog directory, and Nyl publishes its manifest
under `_nyl/shared/previews/`. Every target on the revision must share it. See
[One catalog for every instance](/nyl/orchestration/resource-guides/environment-template/#one-catalog-for-every-instance).

## Reconciler

Every target declares who applies its Releases in `spec.reconciler`, which has
exactly one member and no default:

```yaml
spec:
  reconciler:
    argocd: {instanceRef: {name: dev}}   # an explicit ArgoCDInstance
    # argocd: {}                         # the implicit target-local instance
    # direct: {}                         # Nyl applies the Releases itself
```

`argocd` has Argo CD sync the published tree. Its `instanceRef` names a project-local
[ArgoCDInstance](/nyl/kubernetes-gitops/resource-guides/argocd-instance/) and is
required once any explicit instance exists; with none declared, write `argocd: {}`
and the target gets an implicit instance in its workload Cluster and the `argocd`
namespace. `direct` has Nyl apply the target's Releases to its Cluster itself, and
the target then generates no catalog Application, AppProject, or Application. It is
exclusive with `catalogApplication`. See
[Apply directly without Argo CD](/nyl/kubernetes-gitops/workflow/direct-reconciler/).

A missing or empty `reconciler` fails with `NYL-TARGET-RECONCILER-MISSING`, naming
both forms. Naming both members fails with `NYL-TARGET-RECONCILER-AMBIGUOUS`.

### Migrating from `argocdRef`

The top-level `spec.argocdRef` was removed. A target that still sets it fails with
`NYL-TARGET-ARGOCD-REF-REMOVED`. Move the reference under the reconciler:

```yaml
# before
spec:
  argocdRef: {name: dev}

# after
spec:
  reconciler:
    argocd: {instanceRef: {name: dev}}
```

A target without `argocdRef` and without `reconciler` was an Argo CD target with the
implicit instance; write `reconciler: {argocd: {}}`.

## Release inputs

`spec.releaseInputs` binds the typed inputs of the Releases the target renders,
keyed by `<applicationGroup>/<release>`. Every binding sets exactly one source:
`value`, `fromFile`, `fromGit`, or `fromPublication`; `fromUnit` and
`fromPromotion` are reserved for orchestration and rejected here. Nyl resolves
and checks every binding of the target before rendering, and reports all
problems together; bindings of a selected but disabled ApplicationGroup are
ignored. `target.spec.releaseInputs` is not part of the template context. See
[Release inputs](/nyl/kubernetes-gitops/release-inputs/).

## Publication models

Targets may share a revision when their path prefixes are disjoint. They may
also use separate revisions or repositories. Nyl rejects overlapping prefixes
on the same repository and normalized branch revision, including overlap
through read or write URL aliases.

See [Targets and cluster variation](/nyl/kubernetes-gitops/targets-and-clusters/)
for common deployment topologies.
