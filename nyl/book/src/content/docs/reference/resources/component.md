---
title: 'Component'
---

`Component` is a chart-backed resource format identified by:

- `apiVersion: components.nyl.niklasrosenstein.github.com/v1`
- dynamic `kind` (not a fixed literal)

Unlike fixed resource kinds, `kind` encodes either:

- a local component path (`<apiVersion>/<kind>`)
- a remote shortcut (`<base>[#<name>][@<version>]`)

## Resource Shape

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: example/v1/Nginx
metadata:
  name: web
  namespace: default
spec:
  replicaCount: 2
```

## Semantics

- `kind`: chart target (local or remote shortcut)
- `metadata.name`: Helm release name
- `metadata.namespace`: Helm release namespace (defaults to `default`)
- `spec`: Helm values payload

## Local Resolution

With:

```toml
[project]
components_search_paths = ["components"]
```

`kind: example/v1/Nginx` resolves to:

```text
components/example/v1/Nginx/Chart.yaml
```

Nyl checks `components_search_paths` in order and uses the first match.

## Alias Mapping

`project.aliases` can map regular resource types to component targets:

```toml
[project.aliases]
"myapi.io/v1/MyKind" = "oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4"
```

When an alias matches, Nyl resolves directly to the alias target.

## Remote Shortcut Parsing

Shortcut format:

```text
<base>[#<name>][@<version>]
```

Remote bases:

- `http://` or `https://`
- `oci://`
- `git+`

For complete shortcut examples and guidance, see [Remote Shortcuts & Aliases](/nyl/components/remote-shortcuts-and-aliases/).
