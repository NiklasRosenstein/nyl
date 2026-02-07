# Helm Chart Shortcuts Examples

This directory demonstrates the HelmChart shortcut feature, which allows you to use a concise syntax for deploying Helm charts.

## Shortcut Format

Instead of writing the full HelmChart resource:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: release
  namespace: release-namespace
spec:
  chart:
    repository: http://my-chart-repo.org
    name: my-chart
    version: 0.1.0
  values:
    # ...
```

You can use the shortcut format:

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: http://my-chart-repo.org#my-chart@0.1.0
metadata:
  name: release
  namespace: release-namespace
spec:
  # ...
```

## Shortcut Syntax

The `kind` field uses the format: `<repository>[#<name>][@<version>]`

- `<repository>`: Repository URL (for remote charts) or local path
- `#<name>`: Chart name (optional, after '#')
- `@<version>`: Version or Git ref (optional, after '@')

### Remote Repositories

Remote repositories are identified by these URL prefixes:
- `http://` or `https://` - Traditional Helm repositories
- `git+` - Git repositories
- `oci://` - OCI registries

### Local Paths

If the kind doesn't start with a remote repository prefix, it's treated as a local path and resolved through the existing component resolution mechanism.

## Examples

See the YAML files in this directory for concrete examples:
- `http-chart.yaml` - HTTP Helm repository with full specification
- `oci-chart.yaml` - OCI registry chart
- `git-chart.yaml` - Git repository chart
- `http-chart-no-version.yaml` - Chart without version specified
- `local-chart.yaml` - Local component path (existing behavior)
