---
title: 'Cluster'
---

`Cluster` describes one concrete Kubernetes cluster: its Argo CD destination,
the Kubernetes capabilities used for deterministic rendering, cluster-fact
values, and an optional local kubeconfig context.

## Example

```yaml
apiVersion: gitops.nyl.niklasrosenstein.github.com/v1
kind: Cluster
metadata:
  name: primary
spec:
  destination:
    server: https://kubernetes.default.svc
  kubernetes:
    kubeVersion: 1.31.4
    apiVersions:
      - v1
      - apps/v1
  values:
    region: fsn1
    storageClass: local-path
  live:
    context: admin@primary
```

## Fields

| Field | Required | Description |
| --- | --- | --- |
| `metadata.name` | Yes | Project-local concrete cluster identity. |
| `metadata.labels` | No | Labels for organization and tooling. |
| `spec.destination.server` | Conditional | Argo CD destination API server. Mutually exclusive with `name`. |
| `spec.destination.name` | Conditional | Argo CD registered cluster name. Mutually exclusive with `server`. |
| `spec.kubernetes.kubeVersion` | No | Kubernetes version exposed to renderers such as Helm. |
| `spec.kubernetes.apiVersions` | No | API versions exposed during offline rendering. Defaults to an empty list. |
| `spec.values` | No | Arbitrary cluster facts merged into target render values. |
| `spec.live.context` | No | Local kubeconfig context for live commands. |

Exactly one of `destination.server` and `destination.name` is required. Values
such as region, architecture, storage class, and ingress implementation belong
on the Cluster. Deployment intent such as `environment` belongs on a
[`GitOpsTarget`](/nyl/reference/resources/gitops/gitops-target/).

The capability fields can be absent while scaffolding and updating a Cluster,
but target rendering requires `kubeVersion` and at least one `apiVersions`
entry.

## Rendering behavior

Cluster values are recursively overlaid by target values. Templates receive a
sanitized Cluster as `cluster`; the `live` block is omitted. Kubernetes
capabilities are committed so CI can render deterministically without cluster
access.

`live.context` does not participate in render hashes and is never exposed to
templates or generated manifests.

## Live context resolution

Live commands resolve a context in this order:

1. the command's `--context` option;
2. `spec.live.context`; and
3. the current kubeconfig context.

When possible, Nyl verifies the selected context's API server against a
server-based destination. The conventional
`https://kubernetes.default.svc` in-cluster alias cannot be compared with a
local kubeconfig endpoint.

```bash
nyl cluster info primary
nyl cluster update primary
nyl cluster update primary --check
```

See the [`cluster` command reference](/nyl/commands/cluster/) for details.
