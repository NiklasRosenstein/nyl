---
title: 'Using ArgoCDInstance'
---

See the [ArgoCDInstance resource reference](/nyl/reference/resources/k8s.gitops.nyl/v1/argocd-instance/) for the API, example, and field definitions.

## Catalog defaults

Every target emits a parent Application named `<target>-catalog` unless
`DeploymentTarget.spec.catalogApplication.enabled` is false. The parent recursively
syncs `<pathPrefix>/_nyl/catalog` and therefore manages the generated child
Applications, AppProjects, and its own manifest.

The catalog requires manual sync by default. When synchronized, it applies only
out-of-sync resources with Kubernetes server-side apply. Foreground deletion
cascades to catalog resources, and `selfPrunePolicy: Confirm` annotates the
parent with `Prune=confirm`. A target can override the name, project, sync
policy, deletion policy, self-prune policy, labels, and annotations under
`spec.catalogApplication`.

Enable automated catalog synchronization explicitly when the publication
workflow provides the required approval boundary:

```yaml
spec:
  catalogApplicationDefaults:
    syncPolicy:
      automated:
        enabled: true
        prune: false
        selfHeal: true
      syncOptions:
        - ApplyOutOfSyncOnly=true
        - ServerSideApply=true
```

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
