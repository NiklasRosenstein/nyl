---
title: 'Authoring Local Components'
---

Local components are standard Helm charts placed under configured component search roots.

## Directory Contract

Each component must exist at:

```text
<components_search_path>/<apiVersion>/<kind>/
  Chart.yaml
  values.yaml
  templates/
```

With default configuration:

```toml
[project]
components_search_paths = ["components"]
```

A component `example/v1/Nginx` resolves to:

```text
components/example/v1/Nginx/Chart.yaml
```

## Create a New Component

```bash
nyl create component example/v1 Nginx
```

This scaffolds:

```text
components/example/v1/Nginx/
  Chart.yaml
  values.yaml
  values.schema.json
  templates/deployment.yaml
```

## Consume the Component in a Manifest

```yaml
apiVersion: components.k8s.nyl/v1
kind: example/v1/Nginx
metadata:
  name: web
  namespace: default
spec:
  replicaCount: 3
  image:
    repository: nginx
```

`spec` is passed to Helm as values, merged on top of chart defaults from `values.yaml`.

## Inherit Platform Defaults

A `nyl-component.yaml` next to `Chart.yaml` can declare
[value imports](/nyl/kubernetes-rendering/components/value-imports/): selections of the
effective Cluster and DeploymentTarget values that become chart value defaults.
Invocations then supply only their own settings and overrides.

## Helm `lookup`

A component's `spec` is passed as chart values, so [`lookup`](/nyl/kubernetes-rendering/resource-guides/helm-chart/#reusing-generated-objects-with-lookup)
is not configured there. Enable it for every invocation of a local component in its
`nyl-component.yaml` (`lookup: false` states that it is deliberately off):

```yaml
apiVersion: components.nyl/v1alpha1
lookup: true
```

An invocation overrides the file with the annotation `k8s.nyl/helm-lookup: "true"` or `"false"`
(quoted). The annotation works for every Component, including remote shortcuts and aliases,
which have no `nyl-component.yaml`. Any other value fails with
`NYL-HELM-LOOKUP-ANNOTATION-INVALID`. A HelmChart takes `spec.lookup` instead and rejects the
annotation (`NYL-HELM-LOOKUP-ANNOTATION-MISPLACED`).

```yaml
apiVersion: components.k8s.nyl/v1
kind: example/v1/CiliumCa
metadata:
  name: ca
  annotations:
    k8s.nyl/helm-lookup: "true"
```

A component whose chart calls `lookup` with neither setting warns with
`NYL-HELM-LOOKUP-UNDECLARED`; either setting silences it.

## Operational Notes

- Keep component manifests separate from chart templates. `nyl render` accepts a file path, not a directory.
- Use `nyl validate --strict` in CI to catch missing search paths early.
- Prefer stable `apiVersion/kind` naming for internal component APIs.
