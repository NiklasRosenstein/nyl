---
title: 'Release'
---

`Release` names one deployment, defines its default namespace, and can group
multiple manifest files into one render unit. It is removed from rendered
output.

[View the `Release` JSON schema](/nyl/reference/schemas/release.schema.json).

```yaml
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  include:
    - manifests/*.yaml
  additionalNamespaces:
    - monitoring
  stripEmptyMetadataLabels: argocd
  argocd:
    applicationOverride: {}
```

## Fields

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `metadata.name` | Yes | — | Release and generated Application name. |
| `metadata.namespace` | Yes | — | Default destination namespace. |
| `spec.include` | No | `[]` | Additional manifest paths or glob patterns relative to this file. |
| `spec.additionalNamespaces` | No | `[]` | Extra namespaces available to rendered resources. Ownership follows ApplicationGroup namespace policy. |
| `spec.stripEmptyMetadataLabels` | No | Project setting | Controls empty `metadata.labels` removal. |
| `spec.argocd.applicationOverride` | No | — | Partial generated Application override, subject to the group or generator customization policy. |

Namespace names must be valid Kubernetes namespace names and entries in
`additionalNamespaces` must be unique.

## Multi-file releases

Every `include` pattern is evaluated relative to the directory containing the
Release file. Patterns may select nested files with wildcards such as
`manifests/**/*.yaml`.

- Absolute paths, parent traversal, and symbolic links are rejected.
- Matches must be regular `.yaml`, `.yml`, or `.json` files beneath the Release
  directory.
- Every pattern must match at least one additional file.
- Matches are sorted and deduplicated; the Release entry file is never included
  twice.
- An included file cannot contain another Release.

Included files use the normal rendering pipeline and may contain Kubernetes
resources, Components, HelmCharts, RemoteManifests, or render-time policies.
The same bundle behavior is used by `render`, `apply`, `diff`, the Argo CD CMP,
and rendered-tree compilation.

```text
applications/api/
├── release.yaml
└── manifests/
    ├── deployment.yaml
    └── service.yaml
```

```yaml
# applications/api/release.yaml
apiVersion: gitops.nyl/v1
kind: Release
metadata:
  name: api
  namespace: api
spec:
  include:
    - manifests/*.yaml
```

## ApplicationGroup discovery

ApplicationGroups inspect candidate files for a literal, parseable Release
document before rendering. Files without one are ignored. The Release envelope
must therefore be structurally present in source; scalar values may use quoted
value interpolation, but target templating cannot create or remove the Release
document.

Additional files do not need their own Release because `spec.include` assigns
them to the entry file's deployment unit.

## Namespace scope

For rendered GitOps, Nyl validates every explicit resource
`metadata.namespace` and every rendered `Namespace.metadata.name`. The allowed
set is the effective ApplicationGroup destination namespace plus
`spec.additionalNamespaces`.

An owned additional Namespace remains part of the workload Application. When
`ApplicationGroup.spec.namespace.create` is enabled, Nyl synthesizes a missing
owned Namespace using the same lifecycle policy as the effective destination
Namespace. Externally owned namespaces are authorized but never emitted.

When multiple workload Applications consume one namespace,
`ApplicationGroup.spec.sharedNamespaces` must select one Release, a dedicated
Namespace Application, or external ownership. Nyl rejects rendered Namespace
objects that conflict with that selection.

The Kubernetes bootstrap namespaces `default`, `kube-system`, `kube-public`,
and `kube-node-lease` are externally owned when no explicit shared-namespace
owner is configured. An explicit owner declaration overrides this default.

This check prevents accidental scope expansion. The Argo CD AppProject remains
the runtime authorization boundary.

## Argo CD customization

`spec.argocd.applicationOverride` is applied only to fields permitted by the
ApplicationGroup or ApplicationGenerator release-customization policy. Plain
keys replace values. A `+` prefix appends to supported list-valued fields while
policy checks use the canonical field name.

```yaml
spec:
  argocd:
    applicationOverride:
      metadata:
        annotations:
          example.com/owner: api-team
```

Platform-owned source, destination, project, sync-policy, identity, and
finalizer fields cannot be overridden in rendered GitOps. An ApplicationGroup
may allow exact sync-option values through
`spec.releaseCustomization.allowedSyncOptions`; a Release merges them with a
`+syncOptions` key rather than replacing the group policy. Nyl replaces an
existing option with the same key, which permits an approved
`ApplyOutOfSyncOnly=false` or `ServerSideApply=false` exception to the generated
defaults.

## Direct commands and release history

`nyl render` removes the Release and emits the rest of its bundle. `nyl apply`
uses its name and namespace to record revisions in Kubernetes; `nyl diff` uses
the same identity. Direct commands can still render a file without a Release
when their command-specific name and namespace inputs are supplied.

See also:

- [ApplicationGroup](/nyl/reference/resources/gitops/application-group/)
- [ApplicationGenerator](/nyl/reference/resources/application-generator/)
- [`nyl release`](/nyl/commands/release/)
