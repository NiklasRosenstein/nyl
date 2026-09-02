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

## Remote artifact vendoring

Vendoring is a project-wide policy for remote inputs used by every Cluster and
DeploymentTarget in the project:

```toml
[vendor]
mode = "preferred"
path = "vendor"
lfs_threshold_bytes = 1048576
```

- `mode` is required when the section exists. `disabled` ignores the vendor
  snapshot during rendering, `preferred` resolves vendor then the disposable
  source cache then the origin, and `required` permits only a matching
  vendored artifact.
- `path` defaults to `vendor` and must remain beneath the directory containing
  `nyl.toml`. Nyl excludes this subtree from GitOps YAML discovery.
- `lfs_threshold_bytes` defaults to 1 MiB. Helm and Git archives always use Git
  LFS rules; RemoteManifest blobs use LFS at or above this threshold.

The committed lock identifies artifacts by a deterministic fingerprint of the
complete request coordinate. Multiple Releases requesting the same coordinate
share one lock entry and one blob. See [`nyl vendor`](/nyl/commands/vendor/)
for snapshot maintenance.

Deployment values, Kubernetes capabilities, and kube contexts live in
Kubernetes-shaped [Cluster and DeploymentTarget resources](/nyl/reference/resources/gitops/),
not in `nyl.toml`.

## Render inputs

A target-aware render resolves one `DeploymentTarget` and its referenced `Cluster`.
Values merge recursively in this order:

```text
Cluster.spec.values < DeploymentTarget.spec.values
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
