# Component resource example

A Component is a lightweight alternative to writing a full `HelmChart` resource.
Instead of specifying chart references and release metadata inside a `spec`, you
write a minimal manifest whose `kind` is the path to a Helm chart under
`components/`, and whose `spec` becomes the Helm values directly.

## Layout

```
nyl.toml                    # sets components_search_paths = ["components"]
components/
  example/v1/Nginx/                 # a Helm chart; referenced by kind: example/v1/Nginx
    Chart.yaml
    values.yaml
    templates/
      deployment.yaml
manifests/
  nginx.yaml                        # the Component manifest
```

`nyl render` should target the manifest file directly.

## How it works

`manifests/nginx.yaml` declares:

| Field              | Becomes                        |
|--------------------|--------------------------------|
| `kind`             | path to chart under `components/` |
| `metadata.name`    | Helm release name              |
| `metadata.namespace` | Helm release namespace       |
| `spec`             | Helm values (merged over `values.yaml`) |

Nyl resolves the chart, runs `helm template`, and emits the rendered manifests.

## Run

```sh
nyl render manifests/nginx.yaml --offline --kube-version 1.30.0 --kube-api-versions v1
```
