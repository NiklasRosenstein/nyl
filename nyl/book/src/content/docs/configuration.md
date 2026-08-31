---
title: 'Configuration'
---

Nyl loads project settings from `nyl.toml`. It searches from the current
directory upward and resolves relative paths against the directory containing
the file.

## Project settings

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
gitops_scaffold_path = "config"

[project.aliases]
"myapi.io/v1/MyKind" = "oci://mycharts.org/my-kind@1.0.0"
```

- `components_search_paths` defaults to `["components"]`. Each root is scanned
  for `<root>/<apiVersion>/<kind>/Chart.yaml`.
- `helm_chart_search_paths` defaults to `["."]` and controls Helm chart name
  resolution.
- `gitops_scaffold_path` defaults to `"config"` and controls where
  `nyl new resource` and `nyl new gitops` create files. Discovery remains
  project-wide.
- `aliases` maps an API version and kind to a component shortcut or local
  component path.

Deployment values, Kubernetes capabilities, and kube contexts live in
Kubernetes-shaped [Cluster and GitOpsTarget resources](/nyl/reference/resources/gitops/),
not in `nyl.toml`.

## Render inputs

A target-aware render resolves one `GitOpsTarget` and its referenced `Cluster`.
Values merge recursively in this order:

```text
Cluster.spec.values < GitOpsTarget.spec.values
```

Target values win at every conflicting leaf. Templates receive the merged map
as `values`, the sanitized Cluster as `cluster`, and the target as `target`.
`Cluster.spec.live` is local connection configuration and is never exposed to
templates or generated output.

A targetless render receives no `cluster` or `target`. In offline mode, pass
Kubernetes capabilities explicitly when a Helm chart depends on them:

```bash
nyl render --offline \
  --kube-version 1.31.0 \
  --kube-api-versions v1,apps/v1 \
  applications/example.yaml
```

## Path resolution

Given `/home/user/platform/nyl.toml`:

```toml
[project]
components_search_paths = ["components", "/opt/shared-components"]
helm_chart_search_paths = [".", "charts"]
```

Nyl resolves the paths to `/home/user/platform/components`,
`/opt/shared-components`, `/home/user/platform`, and
`/home/user/platform/charts`.

## Validation and schema

```bash
nyl validate
nyl validate --strict
nyl generate schema config
```

The published project schema is available at
[`reference/schemas/nyl.schema.json`](/nyl/reference/schemas/nyl.schema.json).
