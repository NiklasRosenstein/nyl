---
title: 'Using HelmChart'
---

See the [HelmChart resource reference](/nyl/reference/resources/k8s.nyl/v1/helm-chart/) for the API, example, and field definitions.

## Chart Reference Methods

### Local Path

Reference a chart by filesystem path (absolute or relative) using the `name` field:

```yaml
apiVersion: k8s.nyl/v1
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
apiVersion: k8s.nyl/v1
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
apiVersion: k8s.nyl/v1
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
apiVersion: k8s.nyl/v1
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

Cluster and DeploymentTarget values are Nyl template inputs. They are not passed to
Helm implicitly. This keeps strict chart schemas effective and prevents an
unrelated target value from overriding or extending a chart's values.

Pass a target-aware value explicitly from the Nyl template context:

```yaml
spec:
  values:
    environment: "{{ values.environment }}"
```

Helm receives only `HelmChart.spec.values`. Component resources follow the same
rule and pass only their `spec` payload to Helm.

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
apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata:
  name: myapp
  namespace: production
---
apiVersion: k8s.nyl/v1
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
apiVersion: k8s.nyl/v1
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
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
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
- [Release](/nyl/reference/resources/k8s.gitops.nyl/v1/release/) - Release metadata
