---
title: 'Deprecations'
---

Nyl keeps accepting a deprecated form for a while after it changes, warns
whenever it reads one, and lists it here until support ends. Migrate before
the removal release; nothing else changes when you do.

| Deprecated | Use instead | Deprecated in | Removal |
| --- | --- | --- | --- |
| `Cluster` under `apiVersion: k8s.gitops.nyl/v1` | `apiVersion: k8s.nyl/v1` | the release after 0.5.9 | a future release |
| `Release` under `apiVersion: k8s.gitops.nyl/v1` | `apiVersion: k8s.nyl/v1` | the release after 0.5.9 | a future release |
| `nyl render --context` | nothing: `render` never connects to Kubernetes | the release after 0.5.9 | a future release |

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
