# HelmChart

The HelmChart resource enables declarative Helm chart deployment with templating support. Charts can be referenced from local paths, chart names, or Git repositories.

Use `HelmChart` when you want explicit chart fields in `spec.chart.*`.  
Use `Component` when you want compact chart-backed resources with dynamic `kind` and optional alias indirection.

> **Note**: Git chart references are fully supported. Repositories are cloned automatically to a local cache. See the [Git Integration](../../git-integration.md) guide for details.

## Component Shortcut Syntax

For convenience, Nyl provides a shortcut syntax that allows you to reference Helm charts using the Component resource with a compact `kind` format:

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: <repository>[#<name>][@<version>]
metadata:
  name: release-name
  namespace: release-namespace
spec:
  # Helm values go here
```

This is equivalent to the full HelmChart resource but with less boilerplate.

### Shortcut Format

The `kind` field uses the format: `<repository>[#<name>][@<version>]`

- `<repository>`: Repository URL (for remote charts) or local path
- `#<name>`: Chart name (required for HTTP/HTTPS repositories, optional for others)
- `@<version>`: Version or Git ref (required for HTTP/HTTPS and OCI repositories)

**Remote repositories** are identified by URL prefixes:
- `http://` or `https://` - Traditional Helm repositories (requires `#<name>` and `@<version>`)
- `git+` - Git repositories (optional `#<subpath>` and `@<ref>`, defaults to HEAD if version omitted)
- `oci://` - OCI registries (requires `@<version>`)

**Local paths** (without URL prefix) use the existing component resolution mechanism.

### Shortcut Examples

```yaml
# HTTP Helm repository with full specification
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: https://charts.bitnami.com/bitnami#nginx@18.2.4
metadata:
  name: my-nginx
  namespace: default
spec:
  replicaCount: 2
  service:
    type: ClusterIP
```

```yaml
# OCI registry
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: oci://registry-1.docker.io/bitnamicharts/nginx@18.2.4
metadata:
  name: nginx-oci
spec:
  replicaCount: 1
```

```yaml
# Git repository
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: git+https://github.com/prometheus-community/helm-charts#charts/prometheus@prometheus-25.28.0
metadata:
  name: prometheus
  namespace: monitoring
spec:
  server:
    persistentVolume:
      enabled: false
```

```yaml
# Local component (existing behavior)
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: example/v1/MyComponent
metadata:
  name: local-app
spec:
  replicas: 3
```

See the [examples directory](../../../../examples/helm-chart-shortcuts/) for more examples.

## Alias-Based Components (`nyl.toml`)

You can define named component aliases in `nyl.toml` and then use regular `apiVersion` + `kind` in manifests.

```toml
[project.aliases]
"myapi.io/v1/MyKind" = "oci://mycharts.org/my-kind@1.0.0"
```

```yaml
apiVersion: myapi.io/v1
kind: MyKind
metadata:
  name: my-kind-release
  namespace: default
spec:
  # forwarded as Helm values
  replicaCount: 2
```

The alias value uses the same target syntax as the component shortcut:
- remote shortcut (`https://...#chart@version`, `oci://...@version`, `git+...#path@ref`)
- local component path (`example/v1/MyComponent`)

## Choosing A Paradigm

Nyl supports three ways to reference charts/components:

| Paradigm | How you write it | Best for | Main benefit |
|---|---|---|---|
| Full `HelmChart` resource | `kind: HelmChart` + `spec.chart.*` | Explicit platform manifests | Maximum clarity and full chart fields |
| Component shortcut | `apiVersion: components...` + `kind: <shortcut>` | Fast authoring close to chart source | Minimal boilerplate |
| `project.aliases` named kinds | `apiVersion/kind` mapped in `nyl.toml` | Domain-specific APIs and teams | Stable semantic kinds decoupled from chart location |

## Resource Definition

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: string              # Helm release name
  namespace: string         # Target namespace (optional, defaults to "default")
spec:
  chart:                    # Chart reference (choose one method)
    # Universal fields:
    repository: string      # Repository URL (Git, OCI, or Helm)
    name: string            # Universal name field (context-dependent)
    version: string         # Chart version or Git reference

    # Repository types (indicated by protocol prefix):
    # - Git: repository starts with "git+" (e.g., "git+https://...")
    # - OCI: repository starts with "oci://" (e.g., "oci://ghcr.io/...")
    # - Helm: plain HTTPS URL (e.g., "https://charts.example.com")
    # - Local: no repository, name is filesystem path

  values: object            # Chart values (merged with profile values)

  kube_version: string      # Kubernetes version for template rendering
  api_versions: [string]    # Available API versions for rendering
```

## Chart Reference Methods

### Local Path

Reference a chart by filesystem path (absolute or relative) using the `name` field:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    name: ./charts/nginx
```

### Chart Name

Reference a chart by name (without path separators), searched in configured search paths:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    name: nginx
```

Configure search paths in `nyl.toml`:
```toml
[project]
helm_chart_search_paths = ["./charts", "/opt/helm-charts"]
```

### Git Repository

Reference a chart from a Git repository using the `git+` protocol prefix:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: default
spec:
  chart:
    repository: git+https://github.com/bitnami/charts.git
    version: main
    name: bitnami/nginx
```

**Git Parameters:**
- **`repository`** (required): Git repository URL with `git+` prefix (HTTPS or SSH)
- **`version`** (optional): Branch, tag, or commit SHA (default: `HEAD`)
- **`name`** (optional): Subdirectory within the repository containing the chart

**Helm Dependencies:**

Charts from Git repositories with dependencies are automatically handled. If your chart has a `Chart.yaml` with dependencies or a `Chart.lock` file, Nyl will automatically run `helm dependency build` to fetch and build the dependencies before rendering the chart.

**Examples:**

```yaml
# Latest from main branch
chart:
  repository: git+https://github.com/example/charts.git
  version: main
  name: charts/myapp

# Specific version tag
chart:
  repository: git+https://github.com/example/charts.git
  version: v2.1.0
  name: charts/myapp

# Specific commit
chart:
  repository: git+https://github.com/example/charts.git
  version: abc123def456
  name: charts/myapp

# Root of repository (no subpath)
chart:
  repository: git+https://github.com/example/simple-chart.git
  version: main

# SSH URL
chart:
  repository: git+git@github.com:example/charts.git
  version: main
  name: charts/myapp
```

See [Git Integration](../../git-integration.md) for more details on Git support.

## Release Configuration

The Helm release is configured via the `metadata` fields:

```yaml
metadata:
  name: myapp           # Helm release name
  namespace: production # Target namespace
```

**Defaults:**
- `namespace`: Uses `default` if not specified

### Creating Namespaces

If you need to create the namespace before deploying the chart, add a Namespace resource:

```yaml
apiVersion: v1
kind: Namespace
metadata:
  name: production
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: myapp
  namespace: production
spec:
  chart:
    name: ./charts/myapp
  values:
    replicas: 3
```

When using ArgoCD, you can alternatively enable automatic namespace creation:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
spec:
  syncPolicy:
    syncOptions:
      - CreateNamespace=true
```

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
      tag: "{{ env.NYL_IMAGE_TAG }}"
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
  namespace: production
spec:
  chart:
    repository: git+https://github.com/company/charts.git
    version: v2.1.0
    name: applications/myapp
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
    repository: git+https://github.com/company/charts.git
    version: stable
    name: myapp
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
- [NylRelease](./nyl-release.md) - Release metadata
