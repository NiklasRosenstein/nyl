# Configuration

nyl project settings are loaded from a single file: `nyl.toml`.

## Configuration File Discovery

nyl searches for `nyl.toml` starting in the current directory and walking up parent directories.

## Configuration Structure

`nyl.toml` supports:
- `[project]` for project settings
- `[profile.values.<name>]` for profile values used in templates

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]

[profile.values.default]
namespace = "default"
replicas = 1

[profile.values.dev]
namespace = "dev"
replicas = 2

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

### `project.aliases`

- Type: table/map of string to string
- Default: empty table
- Key format: `<apiVersion>/<kind>`
- Value format: same component shortcut format accepted in `kind` (`<repository>[#<name>][@<version>]`) or a local component path
- Meaning: Treat matching resources as component-style resources and resolve them directly to the configured target instead of `components_search_paths`.

### `profile.values.<name>`

- Type: object/map
- Default: none
- Meaning: Template values for profile `<name>` exposed as `values.*` during rendering.
- Selection: `--profile <name>` selects a profile. If omitted, Nyl uses `default` when available.

Example:

```toml
[profile.values.dev]
my_value = "Hello!"
replicas = 1

[profile.values.prod]
my_value = "World!"
replicas = 3
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
- [`reference/schemas/nyl.schema.json`](./reference/schemas/nyl.schema.json)
