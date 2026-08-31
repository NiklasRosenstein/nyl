---
title: 'ApplicationGroup'
---

`ApplicationGroup` selects targets and `Release` sources, assigns an Argo CD
project, and defines platform-owned Application and Namespace policy.

## Example

```yaml
apiVersion: gitops.nyl/v1
kind: ApplicationGroup
metadata:
  name: workloads
spec:
  targetSelector:
    matchLabels:
      environment: production
  projectRef: workloads
  applicationNamespace: argocd
  source:
    path: applications/workloads
  destinationNamespace: workloads
  namespace:
    create: true
    prunePolicy: Confirm
    deletePolicy: Confirm
  applicationDeletionPolicy: Foreground
  releaseCustomization:
    allowedPaths:
      - metadata.annotations.**
    deniedPaths:
      - spec.project
      - spec.source.**
      - spec.destination.**
```

## Core fields

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `metadata.name` | Yes | — | Group identity and default output directory. |
| `metadata.labels` | No | `{}` | Labels for the control resource itself. |
| `spec.enabled` | No | `true` | Enables the group. May be structurally templated per target. |
| `spec.targetSelector.matchLabels` | No | `{}` | Requires equal target metadata labels. |
| `spec.projectRef` | Yes | — | Local AppProjectDefinition identity. |
| `spec.applicationNamespace` | Yes | — | Namespace containing generated Argo CD Applications. |
| `spec.destinationNamespace` | No | Release namespace | Destination namespace for generated Applications. |
| `spec.outputPath` | No | Group name | Relative directory below the target prefix. |
| `spec.applicationNameTemplate` | No | Release name | Template for generated Application names; receives `release`. |
| `spec.labels` | No | `{}` | Labels added to generated Applications. |
| `spec.annotations` | No | `{}` | Annotations added to generated Applications. |

The group applies only when it is enabled, its project is permitted by the
target's `projects` list, and its target selector matches.

## Source selection

`spec.source` is optional. Without it, a central group derives
`applications/<group-name>`, while `_application-group.yaml` derives its
containing directory.

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `source.path` | For explicit sources | — | Normalized project-relative path. For remote sources, relative to the checkout. |
| `source.repositoryRef.name` | For referenced remote sources | — | Local GitRepository identity. |
| `source.repository` | For inline remote sources | — | Inline `repoURL` and optional `publishURL`. |
| `source.revision` | For remote sources | — | Human-readable Git revision updated by `nyl source update`. |
| `source.commit` | For remote sources | — | Authoritative full immutable commit lock. |
| `source.include` | No | `['*.yaml', '*.yml']` | Relative glob patterns included from the source. |
| `source.exclude` | No | `[]` | Relative glob patterns excluded after inclusion. |
| `source.recursive` | No | `true` | Searches below the source directory when enabled. |
| `source.rendererConfig.mode` | No | `Central` | `Central` uses platform configuration; `Remote` loads the remote project. |
| `source.rendererConfig.projectPath` | No | `.` | Remote project root; valid only in `Remote` mode. |

`repositoryRef` and `repository` are mutually exclusive. A repository makes the
source remote and requires both `revision` and `commit`. `Remote` renderer mode
requires a remote source. Run `nyl source update` to refresh commit locks.

Source selectors identify candidate entry files. Nyl renders only candidates
containing a literal, parseable `gitops.nyl/v1` Release document; other files
are ignored. Use `Release.spec.include` to attach additional relative files or
glob matches to that release.

## Sync and lifecycle policy

| Field | Required | Default | Description |
| --- | --- | --- | --- |
| `spec.syncPolicy.automated.enabled` | No | `false` | Sets Argo CD automated sync enablement. |
| `spec.syncPolicy.automated.prune` | No | `false` | Enables automated pruning. |
| `spec.syncPolicy.automated.selfHeal` | No | `false` | Enables automated self-healing. |
| `spec.syncPolicy.syncOptions` | No | `[]` | Additional Argo CD Application sync options. |
| `spec.applicationDeletionPolicy` | No | `Foreground` | `Foreground`, `Background`, or `Orphan`. |
| `spec.namespace.create` | No | `true` | Synthesizes a missing destination Namespace. |
| `spec.namespace.prunePolicy` | No | `Confirm` | `Automatic`, `Confirm`, or `Retain`. |
| `spec.namespace.deletePolicy` | No | `Confirm` | `Automatic`, `Confirm`, or `Retain`. |

Foreground and Background add the corresponding Argo CD resources finalizer.
Orphan omits it. For Namespace policy, `Confirm` writes `Prune=confirm` or
`Delete=confirm`; `Retain` writes `Prune=false` or `Delete=false`; `Automatic`
adds no restriction.

Each managed Namespace is owned by one dedicated generated Application. Nyl
rejects conflicting project, destination, lifecycle, or metadata policy for the
same cluster/namespace identity. A Release may target its effective destination
namespace plus `Release.spec.additionalNamespaces`. An approved additional
Namespace uses the same dedicated Application and lifecycle policy when the
Release renders it, but is never synthesized from the allow-list alone.

## Per-release Application customization

`spec.releaseCustomization.allowedPaths` and `deniedPaths` are dotted glob
lists applied to `Release.spec.argocd.applicationOverride`. `*` matches one
path segment and `**` matches multiple segments. Deny wins.

Core identity, finalizers, project, sources, destination, and sync policy are
always platform-owned and cannot be customized, even when an allowed pattern
matches. With no allowed paths, release overrides are rejected.

## Structural templating

The ApplicationGroup `spec` can vary per target or render to no document to
omit the group. Its API version, kind, and metadata name remain static. Remote
source coordinates and commit locks must remain statically parseable for
`nyl source update`.

## See also

- [Project structure and discovery](/nyl/deployment-workflows/rendered-manifests/project-structure/)
- [Targets and cluster variation](/nyl/deployment-workflows/rendered-manifests/targets-and-clusters/)
- [Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/)
