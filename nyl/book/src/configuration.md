# Configuration

nyl project settings are loaded from a single file: `nyl.toml`.

## Configuration File Discovery

nyl searches for `nyl.toml` starting in the current directory and walking up parent directories.

## Configuration Structure

`nyl.toml` supports one section: `[project]`.

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
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
