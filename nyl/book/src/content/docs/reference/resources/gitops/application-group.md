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
  sharedNamespaces:
    monitoring:
      owner:
        kind: Release
        applicationGroup: workloads
        release: metrics
    kube-system:
      owner:
        kind: External
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
| `spec.sharedNamespaces` | No | `{}` | Explicit ownership policy for namespaces consumed by more than one workload Application. |

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
are ignored. It warns for each candidate that has no literal Release and is not
claimed by another Release's `spec.include`. Use `Release.spec.include` to
attach additional relative files or glob matches to that release.

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

For a namespace consumed by one workload Application, the Namespace object is
part of that Application's rendered resources. Nyl synthesizes a missing
destination Namespace when `spec.namespace.create` is enabled. An additional
namespace is never synthesized from `Release.spec.additionalNamespaces`; it is
managed only when the Release renders the Namespace object.

Every namespace consumed by more than one workload Application must have an
identical `spec.sharedNamespaces.<namespace>.owner` declaration in every
contributing ApplicationGroup. The owner kinds are:

| Owner kind | Required fields | Behavior |
| --- | --- | --- |
| `Release` | `applicationGroup`, `release` | The selected workload Application owns the Namespace object. Nyl synthesizes it when it is that Release's destination namespace and namespace creation is enabled. |
| `Dedicated` | `applicationGroup` | Nyl synthesizes the Namespace in a separate generated Application using the selected group's project, destination, metadata, sync, and lifecycle policy. |
| `External` | — | Nyl emits no Namespace object. Use this for namespaces managed outside the rendered tree, such as `kube-system`. |

`Release` ownership rejects Namespace objects rendered by other Releases.
`Dedicated` and `External` ownership reject Namespace objects rendered by any
workload Release. These checks prevent two Argo CD Applications from claiming
the same resource instead of silently discarding an authored Namespace.

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
