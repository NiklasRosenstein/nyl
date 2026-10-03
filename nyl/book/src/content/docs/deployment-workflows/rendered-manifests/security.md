---
title: 'Trust and Admission Boundaries'
---

Nyl is a compiler and publisher. It validates the authoring contract, renders a
deterministic tree, and records file ownership and provenance. It is not the
ultimate admission boundary.

## Enforcement layers

- The Git forge controls who may change source resources, platform policy, CI
  definitions, and protected deployment revisions.
- Argo CD controls which repositories, AppProjects, destinations, and sync
  options are accepted during reconciliation.
- Kubernetes authentication, authorization, and admission control which
  resources may enter a cluster.

Keep `Cluster`, `ArgoCDInstance`, `DeploymentTarget`, `AppProjectDefinition`, `ApplicationGroup`, CI
definitions, and protected deployment revisions under platform-owner review.
The forge and Argo CD must enforce that separation when application authors can
change source repositories.

## Application policy

An ApplicationGroup owns generated Application fields such as the project,
source, destination, lifecycle, labels, and annotations. Use
`releaseCustomization.allowedPaths` and `deniedPaths` to expose only deliberate
per-release Application overrides. Deny wins when both lists match. Use
`releaseCustomization.allowedSyncOptions` to approve exact sync-option values
that Releases may merge without delegating the rest of the sync policy. Values
with the same option key replace the generated or group-level value.

AppProject source and destination policy remains an Argo CD enforcement layer
even when an application source repository is compromised. An
`AppProjectDefinition` with `management: External` references an administrator-
managed project without publishing its manifest.

A group that declares no project gets an implied least-privilege AppProject: the
target publication repository, the target workload Cluster with the group's
Release namespaces, and exactly the resource kinds the group renders, split into
cluster-scoped and namespaced whitelists. The project changes with the rendered
output, so a Release that adds a ClusterRole also adds it to the project in the
same render.

`ApplicationGroup.spec.projectTemplate` mirrors the AppProject spec fields;
declared fields are kept as written and omitted fields take the derived values.
Declare a field to make it a fixed boundary that rendered output cannot widen.
With declared `destinations`, Nyl checks every Release namespace against the
entries admitting the target workload Cluster, and adds Namespace permissions
only when namespace creation is enabled and the cluster-resource whitelist is
declared.

Deriving a whitelist needs each rendered kind's scope. It comes from a CRD
rendered in the same target or from the Cluster's recorded capabilities
(`clusterScopedKinds` and vendored CRDs); when neither knows a kind, rendering
fails with a capture hint rather than guessing. The generated policy complements, but
does not replace, Argo CD admission.

The generated parent catalog Application is platform configuration. Its default
automated sync self-heals without automated prune, foreground deletion cascades
to catalog resources, applies only out-of-sync resources, and pruning the parent
itself requires confirmation.
Review and protect the publication path because the parent can create Argo CD
Applications and AppProjects.

## Namespace and deletion policy

Missing destination Namespaces are created by default in the workload
Application that uses them. Namespace pruning and deletion require explicit
Argo CD confirmation by default. A group can select automatic deletion or
retention independently for prune and Application deletion.

Shared namespaces require an explicit, identical ownership declaration in
every contributing ApplicationGroup. Ownership can stay with one selected
Release, move to a dedicated Namespace Application, or remain external to Nyl.
The Kubernetes bootstrap namespaces are implicitly external unless an explicit
owner overrides that default. Nyl rejects conflicting declarations and
Namespace objects rendered by a non-owner.

Generated workload Applications use Argo CD's foreground resources finalizer
by default, so deleting an Application cascades to its resources. Background
deletion and orphaning are explicit alternatives. A dedicated Namespace owner
uses the selected ApplicationGroup's project, destination, lifecycle, and
metadata policy.

See the [ApplicationGroup reference](/nyl/deployment-workflows/rendered-manifests/resource-guides/application-group/#sync-and-lifecycle-policy)
for the exact controls and defaults.

## Remote source isolation

Remote rendering exposes neither a secrets provider nor process environment
variables. `Cluster.spec.live` is omitted from template context. Remote
checkouts and renderer project paths may not contain symbolic links or escape
the checkout root.

A remote source uses a mutable revision for maintainability and a full commit
lock for reproducibility. Rendering consumes the immutable commit. Updating a
lock requires a successful refresh of the mutable revision, so a stale cache is
not accepted as current state.

## Publication integrity

GitOps resources contain credential-free repository coordinates. HTTP URLs
with embedded user information are rejected. Authentication stays in Git
credential configuration and CI secrets.

The rendered index limits reconciliation to files owned by one target. Nyl
preserves unowned files, verifies recorded hashes, rejects overlapping target
prefixes on one repository revision, and uses compare-and-swap publication to
avoid overwriting concurrent updates.

These checks protect compiler and publication invariants. Branch protection,
required review, Argo CD AppProject restrictions, and Kubernetes admission
remain necessary controls.
