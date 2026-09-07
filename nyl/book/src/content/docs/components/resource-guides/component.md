---
title: 'Using Component'
---

See the [Component resource reference](/nyl/reference/resources/components.k8s.nyl/v1/component/) for the API, example, and field definitions.

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
