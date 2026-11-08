---
title: 'Deprecations'
---

Nyl keeps accepting a deprecated form for a while after it changes, warns
whenever it reads one, and lists it here until support ends. Migrate before
the removal release; nothing else changes when you do.

| Deprecated | Use instead | Deprecated in | Removal |
| --- | --- | --- | --- |
| `Cluster` under `apiVersion: k8s.gitops.nyl/v1` | `apiVersion: k8s.nyl/v1` | 0.6.0 | a future release |
| `Release` under `apiVersion: k8s.gitops.nyl/v1` | `apiVersion: k8s.nyl/v1` | 0.6.0 | a future release |
| `nyl render --context` | nothing: `render` never connects to Kubernetes | 0.6.0 | a future release |
| `nyl.niklasrosenstein.github.com/apply-policy-scope` annotation on Kyverno policies | `k8s.nyl/apply-policy-scope` | 0.7.0 | a future release |
| Release state Secrets labelled `nyl.niklasrosenstein.github.com/release` and `nyl.niklasrosenstein.github.com/revision` | `k8s.nyl/release` and `k8s.nyl/revision`, which `nyl apply` records | 0.7.0 | a future release |
| Published baselines whose catalog carries `nyl.niklasrosenstein.github.com/publication-context` | `k8s.gitops.nyl/publication-context`, which publishing the target again records | 0.7.0 | a future release |

## Cluster and Release in `k8s.nyl/v1`

A Cluster describes a Kubernetes cluster and a Release is the unit Nyl renders,
whether `nyl render` and `nyl apply` run it or a deployment target publishes
it. Both belong with the other Kubernetes resources in `k8s.nyl/v1`. Change
the `apiVersion` line of each Cluster and Release document:

```yaml
apiVersion: k8s.nyl/v1
kind: Cluster
```

Rendered output stays the same either way. Schema URLs under
`k8s.gitops.nyl/v1/` keep resolving to the current schemas.

## Labels and annotations in the `.nyl` API groups

Nyl names its labels, annotations, and Secret types after the `.nyl` API group
of the layer that owns them, like its resources: `k8s.nyl/` for Kubernetes
rendering and `k8s.gitops.nyl/` for Kubernetes GitOps.

Rename the scope annotation on each Kyverno policy Nyl applies:

```yaml
metadata:
  annotations:
    k8s.nyl/apply-policy-scope: Global
```

Release state needs no action. `nyl apply` records new revisions under
`k8s.nyl/release` and `k8s.nyl/revision` and keeps reading revisions recorded
under the old labels, with a warning, until the release history limit removes
them. Keep the history limit set, or delete the old revision Secrets after a
successful apply, before support ends.

Published catalog Applications and AppProjects carry
`k8s.gitops.nyl/publication-context`. `diff-tree` and `publish-tree` still read
the old annotation from a baseline, with a warning, so the first publication
after upgrading checks cleanup scope as before and records the new key. Publish
each target once before support ends.

Expanded manifests rendered with `--track-parent` carry
`k8s.nyl/parent-api-version`, `k8s.nyl/parent-kind`, `k8s.nyl/parent-name`, and
`k8s.nyl/parent-namespace`. Nyl only writes these, so they change without a
deprecation.
