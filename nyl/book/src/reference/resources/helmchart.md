# HelmChart

The HelmChart resource enables declarative Helm chart deployment with templating support. Charts can be referenced from local paths, chart names, or Git repositories.

> **Note**: Git chart references are fully supported. Repositories are cloned automatically to a local cache. See the [Git Integration](../../git-integration.md) guide for details.

## Resource Definition

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: string              # Chart instance name
spec:
  chart:                    # Chart reference (choose one method)
    # Local path or chart name
    path: string            # Local filesystem path
    name: string            # Chart name (searched in search paths)

    # Git repository
    git: string             # Git repository URL
    version: string         # Branch, tag, or commit (default: HEAD)
    path: string            # Subdirectory within repository

    # Helm repository
    repository: string      # Helm repository URL
    name: string            # Chart name in repository
    version: string         # Chart version

  release:                  # Optional release configuration
    name: string            # Release name (default: metadata.name)
    namespace: string       # Target namespace

  values: object            # Chart values (merged with profile values)

  kube_version: string      # Kubernetes version for template rendering
  api_versions: [string]    # Available API versions for rendering
```

## Chart Reference Methods

### Local Path

Reference a chart by filesystem path (absolute or relative):

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
spec:
  chart:
    path: ./charts/nginx
  release:
    name: nginx
    namespace: default
```

### Chart Name

Reference a chart by name, searched in configured search paths:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
spec:
  chart:
    name: nginx
  release:
    name: nginx
    namespace: default
```

Configure search paths in `nyl.toml`:
```toml
[settings]
search_path = ["./charts", "/opt/helm-charts"]
```

### Git Repository

Reference a chart from a Git repository:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
spec:
  chart:
    git: https://github.com/bitnami/charts.git
    version: main
    path: bitnami/nginx
  release:
    name: nginx
    namespace: default
```

**Git Parameters:**
- **`git`** (required): Git repository URL (HTTPS or SSH)
- **`version`** (optional): Branch, tag, or commit SHA (default: `HEAD`)
- **`path`** (optional): Subdirectory within the repository containing the chart

**Examples:**

```yaml
# Latest from main branch
chart:
  git: https://github.com/example/charts.git
  version: main
  path: charts/myapp

# Specific version tag
chart:
  git: https://github.com/example/charts.git
  version: v2.1.0
  path: charts/myapp

# Specific commit
chart:
  git: https://github.com/example/charts.git
  version: abc123def456
  path: charts/myapp

# Root of repository (no path)
chart:
  git: https://github.com/example/simple-chart.git
  version: main
```

See [Git Integration](../../git-integration.md) for more details on Git support.

## Release Configuration

The `release` section configures the Helm release:

```yaml
spec:
  release:
    name: myapp           # Helm release name
    namespace: production # Target namespace
```

**Defaults:**
- `name`: Uses `metadata.name` if not specified
- `namespace`: Uses `NylRelease.metadata.namespace` if present, otherwise `default`

## Values

Chart values can be provided in multiple ways:

### Inline Values

```yaml
spec:
  values:
    image:
      repository: nginx
      tag: "1.25"
    replicas: 3
    service:
      type: LoadBalancer
```

### Profile Values

Values from the active profile are automatically merged:

```yaml
# profiles/production.yaml
values:
  replicas: 5
  resources:
    requests:
      cpu: 500m
      memory: 512Mi
```

Inline values take precedence over profile values.

### Templating in Values

Values support Jinja2 templating:

```yaml
spec:
  values:
    image:
      tag: "{{ env.IMAGE_TAG }}"
    environment: "{{ profile.name }}"
```

## Kubernetes Version

Override the Kubernetes version used for template rendering:

```yaml
spec:
  kube_version: "1.28.0"
```

This affects chart templates that use `.Capabilities.KubeVersion`.

## API Versions

Specify available Kubernetes API versions for rendering:

```yaml
spec:
  api_versions:
    - "networking.k8s.io/v1"
    - "autoscaling/v2"
```

This affects chart templates that use `.Capabilities.APIVersions`.

## Complete Example

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: myapp
  namespace: production
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: myapp
spec:
  chart:
    git: https://github.com/company/charts.git
    version: v2.1.0
    path: applications/myapp
  release:
    name: myapp
    namespace: production
  values:
    replicas: 3
    image:
      repository: company/myapp
      tag: "{{ env.VERSION }}"
    ingress:
      enabled: true
      host: myapp.example.com
    resources:
      requests:
        cpu: 500m
        memory: 512Mi
      limits:
        cpu: 1000m
        memory: 1Gi
  kube_version: "1.28.0"
  api_versions:
    - "networking.k8s.io/v1"
```

## Multi-Environment Deployments

Use the same chart with different values per environment:

```yaml
# base manifest
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: myapp
spec:
  chart:
    git: https://github.com/company/charts.git
    version: stable
    path: myapp
  values:
    # Base values here
```

```yaml
# profiles/development.yaml
values:
  replicas: 1
  image:
    tag: latest
  environment: development
```

```yaml
# profiles/production.yaml
values:
  replicas: 5
  image:
    tag: v2.1.0
  environment: production
  resources:
    requests:
      cpu: 1000m
```

Render for specific environment:
```bash
nyl render -e production app.yaml
```

## See Also

- [Git Integration](../../git-integration.md) - Git repository management
- [Configuration](../../configuration.md) - Search paths and settings
- [Profiles](../../configuration.md#profiles) - Environment-specific values
- [NylRelease](./nyl-release.md) - Release metadata
