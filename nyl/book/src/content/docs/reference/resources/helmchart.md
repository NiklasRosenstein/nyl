---
title: 'HelmChart'
---

The HelmChart resource enables declarative Helm chart deployment with templating support. Charts can be referenced from local paths, chart names, or Git repositories.

Use `HelmChart` when you want explicit chart fields in `spec.chart.*`.  
Use [`Component`](/nyl/reference/resources/component/) when you want compact chart-backed resources with dynamic `kind` and optional [alias indirection](/nyl/components/remote-shortcuts-and-aliases/).

> **Note**: Git chart references are fully supported. Repositories are cloned automatically to a local cache. See the [Git Integration](/nyl/git-integration/) guide for details.

## Related: Components

Component resources provide a compact chart-backed format and support local component paths, remote shortcut syntax, and alias-based indirection.

For component syntax, examples, and pattern selection, see:

- [Component resource reference](/nyl/reference/resources/component/)
- [Remote Shortcuts & Aliases](/nyl/components/remote-shortcuts-and-aliases/)

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

  values: object            # Chart-specific values (overlaid by render values)
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

See [Git Integration](/nyl/git-integration/) for more details on Git support.

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

### Cluster and target values

A target-aware render overlays merged Cluster and GitOpsTarget values on the
chart-specific values. The complete precedence is:

```text
HelmChart.spec.values < Cluster.spec.values < GitOpsTarget.spec.values
```

Use chart values for chart-specific defaults, Cluster values for concrete
destination facts, and target values for deployment intent.

### Templating in Values

Values support Jinja2 templating:

```yaml
spec:
  values:
    image:
      tag: "{{ env.NYL_IMAGE_TAG }}"
    environment: "{{ values.environment }}"
```

## Complete Example

```yaml
apiVersion: gitops.nyl/v1
kind: Release
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
apiVersion: gitops.nyl/v1
kind: GitOpsTarget
metadata:
  name: production
spec:
  clusterRef:
    name: primary
  values:
    replicas: 5
    environment: production
    image:
      tag: v2.1.0
    resources:
      requests:
        cpu: 1000m
  publication:
    repository:
      repoURL: https://git.example.com/platform/deploy.git
    revision: deploy/production
```

Render for specific environment:
```bash
nyl render --target production app.yaml
```

## See Also

- [Git Integration](/nyl/git-integration/) - Git repository management
- [Configuration](/nyl/configuration/) - Search paths and settings
- [Release](/nyl/reference/resources/gitops/release/) - Release metadata
