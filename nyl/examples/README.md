# Nyl Examples

These examples cover Kubernetes rendering and orchestration workflows.

## Local Docker with OciImage and OpenTofu

[`oci-image-opentofu`](../../examples/oci-image-opentofu/) builds a web image,
pushes it to a local registry, and passes its digest to OpenTofu to deploy a
container on the local Docker daemon. Its README includes setup and teardown.

## Simple app

[`simple-app`](./simple-app/) demonstrates plain Kubernetes manifests,
target-specific values, a concrete Cluster, and multiple DeploymentTargets that
share the Cluster while publishing to disjoint prefixes.

```bash
cd examples/simple-app
nyl validate
nyl render --target dev manifests/deployment.yaml
nyl render --target prod manifests/deployment.yaml
nyl diff --target dev manifests/deployment.yaml
nyl apply --target dev manifests/deployment.yaml
```

`diff` and `apply` require a reachable cluster. The example Cluster uses the
`kind-kind` kube context; update `config/clusters/local.yaml` for your cluster.

## Learning path

1. Render the simple app for each target and compare the output.
2. Move a value between the Cluster and a target to observe merge precedence.
3. Add a HelmChart resource.
4. Extract repeated manifests into reusable components.
