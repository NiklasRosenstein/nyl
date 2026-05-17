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
nyl new component example/v1 Nginx
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
apiVersion: components.nyl.niklasrosenstein.github.com/v1
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

## Operational Notes

- Keep component manifests separate from chart templates. `nyl render` accepts a file path, not a directory.
- Use `nyl validate --strict` in CI to catch missing search paths early.
- Prefer stable `apiVersion/kind` naming for internal component APIs.
