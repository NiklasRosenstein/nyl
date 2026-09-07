---
title: 'Using Cluster'
---

See the [Cluster resource reference](/nyl/reference/resources/k8s.gitops.nyl/v1/cluster/) for the API, example, and field definitions.

Exactly one of `destination.server` and `destination.name` is required. Values
such as region, architecture, storage class, and ingress implementation belong
on the Cluster. Deployment intent such as `environment` belongs on a
[`DeploymentTarget`](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/).

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
nyl update cluster primary
nyl update cluster primary --check
```

See the [`get and update` command reference](/nyl/commands/project-resources/) for details.
