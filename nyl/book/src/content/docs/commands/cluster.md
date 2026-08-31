---
title: 'cluster'
---

Inspect configured clusters and maintain the Kubernetes capabilities committed
for deterministic rendering.

## `nyl cluster list`

List every discovered Cluster with its Argo CD destination and optional local
kube context:

```bash
nyl cluster list
```

## `nyl cluster info`

Read the live Kubernetes version and API versions for a configured Cluster:

```bash
nyl cluster info primary
nyl cluster info primary --output yaml
nyl cluster info primary --context admin@primary
```

The explicit `--context` wins over `Cluster.spec.live.context`. Without either,
the command uses the current kubeconfig context. When the Cluster destination
uses `server`, Nyl verifies the selected context's API server before connecting.
The conventional `https://kubernetes.default.svc` destination is an in-cluster
alias and cannot be compared with a local kubeconfig endpoint.

## `nyl cluster update`

Refresh `spec.kubernetes` from the live cluster:

```bash
nyl cluster update primary
nyl cluster update primary --context admin@primary
nyl cluster update primary --check
```

The update sorts and deduplicates `apiVersions`, changes only
`spec.kubernetes`, and preserves content outside that generated block in a
static Cluster YAML document. `--check` performs no write and exits unsuccessfully
when the committed capabilities differ from the live cluster, which makes it
suitable for CI drift checks.

Cluster capabilities affect rendered output. Review and commit capability
updates like any other platform configuration change.
