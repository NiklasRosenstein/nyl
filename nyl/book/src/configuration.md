# Configuration

nyl uses a project configuration file to define project settings. The configuration file can be in YAML or JSON format.

## Configuration File

nyl searches for configuration files in the following order:

1. `nyl-project.yaml`
2. `nyl-project.json`

The search starts in the current directory and moves upward through parent directories until a configuration file is found.

## Configuration Structure

### YAML Format

```yaml
settings:
  generate_applysets: false
  on_lookup_failure: Error
  components_path: components
  search_path:
    - .
    - lib
```

### JSON Format

```json
{
  "settings": {
    "generate_applysets": false,
    "on_lookup_failure": "Error",
    "components_path": "components",
    "search_path": [
      ".",
      "lib"
    ]
  }
}
```

## Settings

### `generate_applysets`

- **Type**: `boolean`
- **Default**: `false`
- **Description**: If enabled, automatically generate an ApplySet for every template file. (Phase 3+)

### `on_lookup_failure`

- **Type**: `string`
- **Default**: `"Error"`
- **Valid Values**: `"Error"`, `"CreatePlaceholder"`, `"SkipResource"`
- **Description**: Behavior when a lookup() call fails during template rendering. (Phase 3+)
  - `Error`: Fail with an error
  - `CreatePlaceholder`: Create a placeholder resource
  - `SkipResource`: Skip the resource that depends on the lookup

### `components_path`

- **Type**: `string` (path)
- **Default**: `"components"`
- **Description**: Path to the directory that contains nyl components. Relative paths are resolved relative to the configuration file location.

### `search_path`

- **Type**: `array` of `string` (paths)
- **Default**: `["."]`
- **Description**: Search paths for additional resources used by the project. Used for example when using the `chart.path` option on a HelmChart resource. (Phase 2+) Relative paths are resolved relative to the configuration file location.

## Path Resolution

All relative paths in the configuration are resolved relative to the configuration file's parent directory.

For example, if your configuration is at `/home/user/my-app/nyl-project.yaml`:

```yaml
settings:
  components_path: components        # Resolves to /home/user/my-app/components
  search_path:
    - .                              # Resolves to /home/user/my-app
    - lib                            # Resolves to /home/user/my-app/lib
    - /absolute/path                 # Remains /absolute/path
```

## Default Configuration

If no configuration file is found, nyl uses the following defaults:

```yaml
settings:
  generate_applysets: false
  on_lookup_failure: Error
  components_path: null              # Defaults to ./components
  search_path:
    - .
```

## Validation

Use `nyl validate` to check your configuration:

```bash
nyl validate
```

This checks:
- Configuration file syntax (YAML/JSON)
- `on_lookup_failure` has a valid value
- Components directory exists
- Search paths are accessible

Use `--strict` mode in CI/CD to treat warnings as errors:

```bash
nyl validate --strict
```

## Future Settings (Phase 2+)

The configuration file may also contain:

- `profiles`: Profile configurations for different environments
- `secrets`: Secret provider configurations

These sections are ignored in Phase 1 but will be supported in future phases.
