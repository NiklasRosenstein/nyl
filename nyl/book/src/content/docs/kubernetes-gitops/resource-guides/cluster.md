---
title: 'Using Cluster'
---

See the [Cluster resource reference](/nyl/reference/resources/k8s.nyl/v1/cluster/) for the API, example, and field definitions.

Exactly one of `destination.server` and `destination.name` is required. Values
such as region, architecture, storage class, and ingress implementation belong
on the Cluster. Deployment intent such as `environment` belongs on a
[`DeploymentTarget`](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/).

The capability fields can be absent while scaffolding and updating a Cluster,
but target rendering requires `kubeVersion` and at least one `apiVersions`
entry.

`clusterScopedKinds` records which kinds are cluster-scoped, as `group/Kind`
with `core` for the core API group; every other kind in `apiVersions` is
namespaced. When `vendoredCrds` is true, the API versions and scopes of
CustomResourceDefinitions come from the Cluster's vendored CRD snapshot instead
of these lists, and rendering fails while that snapshot is missing or stale.
The recorded scopes are also what [`nyl apply`](/nyl/reference/cli/apply/#which-kinds-are-namespaced),
`nyl diff`, and the direct reconciler use, after the manifest's own CRDs and before
the live cluster, to decide which resources take the destination namespace.

## Rendering behavior

Cluster values are recursively overlaid by target values. Templates receive a
sanitized Cluster as `cluster`; the `live` block is omitted. Kubernetes
capabilities are committed so CI can render deterministically without cluster
access.

`spec.valuesFrom` loads plain YAML mapping files before inline `spec.values`.
See [file-backed values](/nyl/kubernetes-gitops/targets-and-clusters/#file-backed-values)
for paths, merge order, and examples.

`live.context` does not participate in render hashes and is never exposed to
templates or generated manifests.

## Live context resolution

Live commands resolve a context in this order:

1. the command's `--context` option; and
2. `spec.live.context`.

Only when the Cluster declares neither is the current kubeconfig context used,
and then only if the Cluster's destination is a concrete server URL that the
context's server can be checked against; an in-cluster or by-name destination
needs `--context`. This holds for every command that connects through a
Cluster: `nyl apply` and `nyl diff` with `--target`, `nyl release` with
`--target`, and `nyl capture cluster`.

When possible, Nyl verifies the selected context's API server against a
server-based destination. The conventional
`https://kubernetes.default.svc` in-cluster alias cannot be compared with a
local kubeconfig endpoint.

```bash
nyl capture cluster primary
nyl capture cluster primary --check
```

See the [`get and update` command reference](/nyl/reference/cli/project-resources/) for details.

An Argo CD control-plane Cluster's `spec.live.context` is also the context
through which a [`KubernetesPublication` that declares `observe: {}`](/nyl/kubernetes-gitops/resource-guides/argocd-instance/#reading-argo-cd)
reads Argo CD's Applications, when the ArgoCDInstance declares no other access.
That reading never uses `--context` or the current context.
