# validate

Validate project configuration and check for common issues.

## Synopsis

```bash
nyl validate [path] [--strict]
```

## Arguments

- `[path]` - Path to the project directory (default: current directory)

## Options

- `--strict` / `-s` - Treat warnings as errors (useful for CI/CD)

## Description

The `validate` command checks your nyl project configuration for:

1. **Configuration file existence** - Warns if no config file is found
2. **Configuration syntax** - Validates YAML/JSON syntax
3. **Components directory** - Checks if components directory exists
4. **Search paths** - Verifies all search paths are accessible
5. **Settings validation** - Ensures `on_lookup_failure` has a valid value

## Output

### Success

```bash
$ nyl validate
✓ Found project config: /path/to/nyl-project.yaml
✓ Components directory exists: /path/to/components
✓ Search path exists: /path/to/lib

✓ Validation passed
```

### With Warnings

```bash
$ nyl validate
✓ Found project config: /path/to/nyl-project.yaml
⚠ Components directory does not exist: /path/to/components
✓ Search path exists: /path/to/lib

Validation warnings:
  ⚠ Components directory does not exist: /path/to/components

✓ Validation passed with warnings
```

### No Configuration

```bash
$ nyl validate
⚠ No project configuration file found, using defaults
⚠ Components directory does not exist: ./components
✓ Search path exists: .

Validation warnings:
  ⚠ Components directory does not exist: ./components

✓ Validation passed with warnings
```

## Strict Mode

In strict mode, warnings are treated as errors. This is useful for CI/CD pipelines:

```bash
$ nyl validate --strict
✓ Found project config: /path/to/nyl-project.yaml
⚠ Components directory does not exist: /path/to/components
✓ Search path exists: /path/to/lib

Validation warnings:
  ⚠ Components directory does not exist: /path/to/components

✗ Validation failed in strict mode (warnings treated as errors)
Error: Validation failed in strict mode
```

Exit code: 1

## Validation Checks

### 1. Configuration File

- **Check**: Configuration file exists
- **Warning**: No configuration file found (uses defaults)
- **Error**: Configuration file has syntax errors

### 2. Components Directory

- **Check**: Directory specified in `components_path` exists
- **Warning**: Directory does not exist
- **Note**: Creates warning even with default configuration

### 3. Search Paths

- **Check**: All paths in `search_path` exist
- **Warning**: Path does not exist for each missing path

### 4. on_lookup_failure Value

- **Check**: Value is one of: `"Error"`, `"CreatePlaceholder"`, `"SkipResource"`
- **Warning**: Invalid value specified

## Examples

### Validate current project

```bash
nyl validate
```

### Validate specific project

```bash
nyl validate /path/to/project
```

### Validate in CI/CD

```yaml
# .github/workflows/validate.yml
jobs:
  validate:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v2
      - name: Validate nyl project
        run: nyl validate --strict
```

### Validate with verbose logging

```bash
nyl --verbose validate
```

Output includes debug information:
```
INFO Validating project configuration
DEBUG Validation path: .
DEBUG Strict mode: false
✓ Found project config: /path/to/nyl-project.yaml
...
```

## Error Handling

### Invalid YAML

```bash
$ nyl validate
Error: Failed to parse YAML config: invalid YAML syntax
```

### Invalid JSON

```bash
$ nyl validate
Error: Failed to parse JSON config: expected ',' or '}' at line 5
```

### Invalid on_lookup_failure

```bash
$ nyl validate
✓ Found project config: /path/to/nyl-project.yaml

Validation warnings:
  ⚠ Invalid on_lookup_failure value 'InvalidValue'. Must be one of: Error, CreatePlaceholder, SkipResource

✓ Validation passed with warnings
```

## Exit Codes

- `0` - Validation passed (with or without warnings in normal mode)
- `1` - Validation failed (syntax errors or warnings in strict mode)
