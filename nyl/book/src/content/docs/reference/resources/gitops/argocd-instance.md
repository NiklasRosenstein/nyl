---
title: 'ArgoCDInstance'
---

`ArgoCDInstance` models one Argo CD control plane independently from the
workload clusters it manages. Several DeploymentTargets can reference the same
instance while deploying to different Clusters.

## Example

```yaml
apiVersion: gitops.nyl/v1
kind: ArgoCDInstance
metadata:
  name: central
spec:
  clusterRef:
    name: management
  namespace: argocd
  catalogApplicationDefaults:
    project: default
    syncPolicy:
      automated:
        enabled: true
        prune: false
        selfHeal: true
      syncOptions:
        - ApplyOutOfSyncOnly=true
    applicationDeletionPolicy: Foreground
    selfPrunePolicy: Confirm
```

`spec.clusterRef` identifies the Cluster where Argo CD runs. This is distinct
from a DeploymentTarget's workload `clusterRef`. `spec.namespace`, defaulting to
`argocd`, is the namespace containing generated AppProjects and the parent
catalog Application. It is also that parent's destination namespace.

## Catalog defaults

Every target emits a parent Application named `<target>-catalog` unless
`DeploymentTarget.spec.catalogApplication.enabled` is false. The parent recursively
syncs `<pathPrefix>/_nyl/catalog` and therefore manages the generated child
Applications, AppProjects, and its own manifest.

The defaults enable automated sync and self-healing, apply only out-of-sync
resources, and leave automated prune disabled. Foreground deletion cascades to
catalog resources, and
`selfPrunePolicy: Confirm` annotates the parent with `Prune=confirm`. A target
can override the name, project, sync policy, deletion policy, self-prune
policy, labels, and annotations under `spec.catalogApplication`.

When no ArgoCDInstance resources exist, each target gets an implicit local
instance using its workload Cluster, the `argocd` namespace, and these defaults.
Nyl logs that choice. Once any explicit instance exists, every target must set
`spec.argocdRef.name`; this prevents accidental attachment to the wrong control
plane.

The `default` AppProject must exist before the catalog can manage itself. After
rendering and publishing a target for the first time, seed its parent once:

```bash
kubectl apply -f deploy/<target>/_nyl/catalog/applications/argocd/<target>-catalog.yaml
```

Use the actual rendered output path and configured namespace when they differ.

## Schema

[`argocd-instance.schema.json`](/nyl/reference/schemas/argocd-instance.schema.json)
