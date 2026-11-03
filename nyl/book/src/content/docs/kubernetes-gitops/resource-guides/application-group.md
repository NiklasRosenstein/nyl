---
title: 'Using ApplicationGroup'
---

See the [ApplicationGroup resource reference](/nyl/reference/resources/k8s.gitops.nyl/v1/application-group/) for the API, example, and field definitions.

`DeploymentTarget.spec.applicationGroupSelector.matchLabels` matches the group's
static metadata labels. An empty target selector matches every group. After
selection, `spec.enabled` may be structurally templated per target.

## Project assignment

A group that declares neither `projectRef` nor `projectTemplate` owns an
implied AppProject named after the group. It admits only what the group's
Applications deploy: the target publication repository, the target workload
Cluster with the group's Release namespaces, and the resource kinds the group
renders. Nothing outside the group needs to exist for it to render.

`projectRef` retains a reusable AppProjectDefinition contract. A `Rendered`
definition is copied into the target catalog once; an `External` definition
only supplies the project name.

`projectTemplate` generates the group's AppProject. `name`, `labels`,
`annotations`, and `finalizers` set its metadata, and the remaining fields mirror the
[Argo CD AppProject](https://argo-cd.readthedocs.io/en/stable/user-guide/projects/)
spec: `description`, `sourceRepos`, `sourceNamespaces`, `destinations`,
`clusterResourceWhitelist`, `clusterResourceBlacklist`,
`namespaceResourceWhitelist`, `namespaceResourceBlacklist`, `roles`,
`syncWindows`, `orphanedResources`, `signatureKeys`,
`permitOnlyProjectScopedClusters`, and `destinationServiceAccounts`. Declared
fields are kept as written. Every omitted field is derived from the group, so a
bare template only renames the implied project:

```yaml
projectTemplate:
  name: workloads
  sourceRepos:
    - https://git.example.com/deploy.git
  destinations:
    - server: https://kubernetes.default.svc
      namespace: workloads
    - server: https://kubernetes.default.svc
      namespace: 'preview-*'
  clusterResourceWhitelist:
    - group: apiextensions.k8s.io
      kind: CustomResourceDefinition
```

| Omitted field | Generated value |
| --- | --- |
| `sourceRepos` | The target publication repository |
| `destinations` | The target workload Cluster with each Release destination and additional namespace |
| `clusterResourceWhitelist` | The cluster-scoped kinds the group renders, including Namespaces Nyl creates for it |
| `namespaceResourceWhitelist` | The namespaced kinds the group renders |

The whitelists need each rendered kind's scope, which comes from a CRD rendered
in the same target or from the Cluster's recorded capabilities. Recapture a
Cluster with `nyl capture cluster` when rendering reports an unknown scope, or
declare both whitelists.

The name defaults to the ApplicationGroup name. Nyl always adds
`applicationNamespace` to `sourceNamespaces`. Each `destinations` entry names a
`server` or `name` pattern and a `namespace` pattern. A fixed
`destinationNamespace` not covered by an entry for the target workload Cluster
is added for that Cluster. Every effective Release destination and
`additionalNamespaces` entry must match an entry that admits the target
workload Cluster.

A declared `clusterResourceWhitelist`, including `[]`, admits only its
patterns; when namespace creation is enabled, Nyl then adds Namespace
permissions for the target Cluster's admitted namespace patterns. Argo CD's
AppProject admission remains the authorization boundary.

## Application names

Each Release produces one workload Application named after the Release. When
several targets place Applications into the same Argo CD namespace on the same
control-plane Cluster, set `applicationNameTemplate` so their names differ.
Targets on implicit per-target instances of one Cluster share the `argocd`
namespace, so they need it too. Clusters count as one control plane when they
share an Argo CD destination, and `nyl validate` compares the names every
target generates, so a template without the target, or Releases of the same
name in different groups, are reported as well:

```yaml
spec:
  applicationNameTemplate: '${ target.metadata.name }-${ release.metadata.name }'
```

The field is a template value: the structural pass leaves `${ … }` untouched,
and Nyl evaluates each expression once per Release with the target context and
`release`. Expressions and filters work as in `{{ … }}`; blocks do not. An
undefined variable or attribute is an error. Write `$${` for a literal `${`.
All other text is literal, including `{{ … }}`. The field must contain at least
one `${ … }` expression; the former `{% raw %}{{ … }}{% endraw %}` form is no
longer expanded and fails validation, so rewrite it with `${ … }`.

## Source selection

`spec.source` is optional. Without it, a central group derives
`applications/<group-name>`, while `_application-group.yaml` derives its
containing directory.

`repositoryRef` and `repository` are mutually exclusive. A repository makes the
source remote and requires both `revision` and `commit`. `Remote` renderer mode
requires a remote source. Run `nyl update source-locks` to refresh commit locks.

Source selectors identify candidate entry files. Nyl renders only candidates
containing a literal, parseable `k8s.nyl/v1` Release document; other files
are ignored. It warns for each candidate that has no literal Release and is not
claimed by another Release's `spec.include`. Use `Release.spec.include` to
attach additional relative files or glob matches to that release.

## Sync and lifecycle policy

Foreground and Background add the corresponding Argo CD resources finalizer.
Orphan omits it. For Namespace policy, `Confirm` writes `Prune=confirm` or
`Delete=confirm`; `Retain` writes `Prune=false` or `Delete=false`; `Automatic`
adds no restriction.

For a namespace consumed by one workload Application, the Namespace object is
part of that Application's rendered resources. Nyl synthesizes a missing
destination Namespace and every Namespace listed in
`Release.spec.additionalNamespaces` when `spec.namespace.create` is enabled.

The Kubernetes bootstrap namespaces `default`, `kube-system`, `kube-public`,
and `kube-node-lease` implicitly use `owner.kind: External`. Releases may use
them when their namespace scope allows it, but Nyl neither synthesizes nor
accepts an authored Namespace object. An explicit `sharedNamespaces` owner
overrides this default when the platform deliberately delegates ownership.

Every other namespace consumed by more than one workload Application must have
an identical `spec.sharedNamespaces.<namespace>.owner` declaration in every
contributing ApplicationGroup. The owner kinds are:

`Release` ownership rejects Namespace objects rendered by other Releases.
`Dedicated` and `External` ownership reject Namespace objects rendered by any
workload Release. These checks prevent two Argo CD Applications from claiming
the same resource instead of silently discarding an authored Namespace.

## Per-release Application customization

`spec.releaseCustomization.allowedPaths` and `deniedPaths` are dotted glob
lists applied to `Release.spec.argocd.applicationOverride`. `*` matches one
path segment and `**` matches multiple segments. Deny wins.

Every generated directory Application defaults to `ApplyOutOfSyncOnly=true`
and `ServerSideApply=true`. Set an option to `false` in the ApplicationGroup
sync options to override that generated default. A resource can opt out of
server-side apply with the
`argocd.argoproj.io/sync-options: ServerSideApply=false` annotation.

`spec.releaseCustomization.allowedSyncOptions` is an exact allow-list for sync
options that a Release may merge with `spec.syncPolicy.+syncOptions` in its
Application override:

```yaml
apiVersion: k8s.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  argocd:
    applicationOverride:
      spec:
        syncPolicy:
          +syncOptions:
            - RespectIgnoreDifferences=false
```

Every merged value must appear verbatim in `allowedSyncOptions`. Nyl replaces
an existing option with the same key, so an approved
`ApplyOutOfSyncOnly=false` value overrides the generated default without
emitting conflicting entries. A plain
`syncOptions` key attempts replacement and remains forbidden.

Core identity, finalizers, project, sources, destination, and sync policy other
than explicitly allowed sync-option additions are platform-owned and cannot be
customized, even when an allowed path pattern matches. With no allowed paths,
ordinary release overrides are rejected.

## Structural templating

The ApplicationGroup `spec` can vary per target or render to no document to
omit the group. Its API version, kind, and metadata name remain static. Remote
source coordinates and commit locks must remain statically parseable for
`nyl update source-locks`.

## See also

- [Project structure and discovery](/nyl/kubernetes-gitops/project-structure/)
- [Targets and cluster variation](/nyl/kubernetes-gitops/targets-and-clusters/)
- [Trust and admission boundaries](/nyl/kubernetes-gitops/security/)

## Retiring Applications safely

Argo CD synchronizes AppProject permissions independently of Application pruning.
Disabling automatic catalog pruning does not preserve those permissions. During
cascading deletion, Argo CD skips managed resources outside the Application's
current AppProject scope, which can leave resources orphaned even with a deletion
finalizer. This filtering happens in
[`getPermittedAppLiveObjects()` and `finalizeApplicationDeletion()`](https://github.com/argoproj/argo-cd/blob/master/controller/appcontroller.go).

Keep cleanup namespaces in the group-owned generated project with one additive setting:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: workloads
spec:
  applicationNamespace: argocd
  retainedProjectNamespaces:
    - retired-service
```

`retainedProjectNamespaces` defaults to `[]`. It adds concrete Kubernetes namespace
names to the existing inferred or explicit destinations on the target Cluster.
Duplicates are removed deterministically. It participates in the existing generated
Namespace-management permissions when namespace creation is enabled, and creates no
Namespace, Release, or Application. Wildcards and invalid namespace names are rejected.
The setting keeps project inference enabled and needs no complete `projectTemplate`.
With `projectRef`, retain scope in the owning AppProjectDefinition instead; the
retention setting is rejected for independently managed and external projects.

Retention does not override explicit destination denies, resource blacklists, or
other restrictions. If retiring a workload also removes the last inferred permission
for one of its resource kinds, keep that permission explicitly in
`projectTemplate.namespaceResourceWhitelist` or `clusterResourceWhitelist` as
appropriate. Explicit whitelist fields retain their usual meaning: declare the full
set the project needs. A change of cluster or project identity can also require
retaining the original destination or project declaration.

`diff-tree` and `publish-tree` fail with `NYL_PROJECT_SCOPE_CONTRACTION` when their
baseline contains a cascading Application whose retiring resources lose effective
project permission. The check uses target-owned catalog sources and rendered resources,
including additional namespaces and Namespace objects. Equivalent wildcard permission
is sufficient. Applications configured for orphan deletion do not require cascading
cleanup. External project permissions absent from the catalog and live Argo CD
cluster registration are outside the published snapshots the guard evaluates.

For automatic catalog synchronization with manual Application pruning:

1. Remove or replace the Application in source. Run `diff-tree` in MR CI to detect
   simultaneous cleanup permission contractions.
2. Add each reported namespace to `retainedProjectNamespaces`, preserving existing
   entries, and retain any additional project permissions the diagnostic identifies.
3. Publish. Let Argo CD synchronize the catalog and project permissions.
4. Manually prune the retiring Application from the catalog, then complete any
   required resource deletion confirmations.
5. Verify live cascading deletion has completed before removing retention entries
   and publishing the narrower project.

**This is a publication-transition check, not confirmation of live deletion.** Once
an Application is absent from the comparison baseline, the guard cannot establish
whether its live deletion has completed. A successful publication does not prove
there are no orphaned resources. Operators own that verification. Retained storage
remains governed by its reclaim and deletion policies.

The check needs no live Kubernetes access and keeps no durable retirement state.
It does not change automatic pruning, finalizers, or deletion policies. For a
reviewed exception, both commands accept `--ignore-project-scope-contraction` and
print the findings as a warning; baseline integrity errors still fail.
