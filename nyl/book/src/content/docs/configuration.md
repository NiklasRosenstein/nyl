---
title: 'Configuration'
---

nyl project settings are loaded from a single file: `nyl.toml`.

## Configuration File Discovery

nyl searches for `nyl.toml` starting in the current directory and walking up parent directories.

## Configuration Structure

`nyl.toml` supports:
- `[project]` for project settings
- `[project.kubernetes]` for default offline render Kubernetes metadata
- `[profile.<name>.values]` for profile values used in templates
- `[profile.<name>.kubernetes]` for profile-specific offline render Kubernetes metadata

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
gitops_scaffold_path = "config"

[project.kubernetes]
kube_version = "1.30.0"
api_versions = ["v1", "apps/v1", "batch/v1"]

[profile.default.values]
namespace = "default"
replicas = 1

[profile.dev.values]
namespace = "dev"
replicas = 2

[profile.dev.kubernetes]
kube_version = "1.30.0"
api_versions = ["v1", "apps/v1", "networking.k8s.io/v1"]

[project.aliases]
"myapi.io/v1/MyKind" = "oci://mycharts.org/my-kind@1.0.0"
```

## Settings

### `project.components_search_paths`

- Type: array of path strings
- Default: `["components"]`
- Meaning: Direct roots for component charts. Each root is scanned as:
  - `<root>/<apiVersion>/<kind>/Chart.yaml`

### `project.helm_chart_search_paths`

- Type: array of path strings
- Default: `["."]`
- Meaning: Search paths used for Helm chart name resolution.

### `project.gitops_scaffold_path`

- Type: path string
- Default: `"config"`
- Meaning: Destination root used by `nyl new resource` and `nyl new gitops`.
  GitOps resource discovery remains project-wide and is not restricted to this
  directory.

### `project.aliases`

- Type: table/map of string to string
- Default: empty table
- Key format: `<apiVersion>/<kind>`
- Value format: same component shortcut format accepted in `kind` (`<repository>[#<name>][@<version>]`) or a local component path
- Meaning: Treat matching resources as component-style resources and resolve them directly to the configured target instead of `components_search_paths`.

### `project.kubernetes`

- Type: object/table
- Default: empty
- Meaning: Default Kubernetes target settings.
- Typical use: Commit `kube_version` / `api_versions` when using [rendered manifest GitOps](/nyl/deployment-workflows/rendered-manifests/) so CI can render deterministically without connecting to a cluster.
- Fields:
  - `kube_version`: Kubernetes version passed to Helm during offline rendering, for example `"1.30.0"`.
  - `api_versions`: array of Kubernetes API versions passed to Helm during offline rendering, for example `["v1", "apps/v1"]`.
  - `context`: target kube context to connect to for live operations (`render`, `diff`, `apply`, `cluster-info`). When set, Nyl uses this context instead of the current kubeconfig context, guarding against accidentally running against the wrong cluster.

#### Target context precedence

The kube context is resolved as: `--context` CLI flag > `profile.<name>.kubernetes.context` > `project.kubernetes.context` > the current kubeconfig context.

```toml
[project.kubernetes]
context = "dev-cluster"

[profile.prod.kubernetes]
context = "prod-cluster"
```

### `profile.<name>.values`

- Type: object/map
- Default: none
- Meaning: Template values for profile `<name>` exposed as `values.*` during rendering.
- Selection: `--profile <name>` selects a profile. If omitted, Nyl uses `default` when available.

Example:

```toml
[profile.dev.values]
my_value = "Hello!"
replicas = 1

[profile.prod.values]
my_value = "World!"
replicas = 3
```

### `profile.<name>.kubernetes`

- Type: object/table
- Default: empty
- Meaning: Profile-specific Kubernetes target settings (same fields as `project.kubernetes`, including `context`).
- Precedence: CLI flags override profile config, profile config overrides `project.kubernetes`.

Example:

```toml
[profile.prod.kubernetes]
kube_version = "1.30.0"
api_versions = ["v1", "apps/v1", "batch/v1", "networking.k8s.io/v1"]
context = "prod-cluster"
```

Template usage:

```jinja
{{ values.my_value }}
{% if profile == "dev" %}
# dev-specific logic
{% endif %}
```

Example:

```toml
[project]
components_search_paths = ["components"]

[project.aliases]
"myapi.io/v1/MyKind" = "oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4"
"platform.example.io/v1/IngressStack" = "https://charts.bitnami.com/bitnami#nginx@18.2.4"
```

Then this manifest is resolved through the alias target:

```yaml
apiVersion: myapi.io/v1
kind: MyKind
metadata:
  name: my-nginx
spec:
  replicaCount: 2
```

## Path Resolution

Relative paths are resolved against the directory that contains `nyl.toml`.

Example (`/home/user/my-app/nyl.toml`):

```toml
[project]
components_search_paths = ["components", "/opt/shared-components"]
helm_chart_search_paths = [".", "charts"]
```

Resolves to:
- `components_search_paths`:
  - `/home/user/my-app/components`
  - `/opt/shared-components`
- `helm_chart_search_paths`:
  - `/home/user/my-app`
  - `/home/user/my-app/charts`

## Validation

Use:

```bash
nyl validate
```

Checks:
- `nyl.toml` discovery and parse validity
- existence of configured `components_search_paths`
- existence of configured `helm_chart_search_paths`

Use strict mode in CI:

```bash
nyl validate --strict
```

## JSON Schema

Generate schema from the current binary:

```bash
nyl generate schema config
```

Published schema artifact:
- [`reference/schemas/nyl.schema.json`](/nyl/reference/schemas/nyl.schema.json)
