# NylRelease

The NylRelease resource specifies release metadata (name and namespace) for a deployment. It's an optional resource that provides context for rendering manifests.

## Resource Definition

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: string        # Release name
  namespace: string   # Target namespace
spec: {}              # Reserved for future use
```

## Field Reference

### metadata.name

- **Type**: `string` (required)
- **Description**: The release name
- **Usage**: Used as the Helm release name when rendering HelmCharts

### metadata.namespace

- **Type**: `string` (required)
- **Description**: The target namespace for this release
- **Usage**: Resources are deployed to this namespace

### spec

- **Type**: `object` (optional)
- **Description**: Reserved for future metadata like labels, annotations
- **Current**: Empty object (`{}`)

## Behavior

### During Rendering

When `nyl render` encounters a NylRelease:

1. The resource is **extracted** from the file
2. Metadata is used for rendering other resources in the file
3. The NylRelease itself is **not included** in the output

### With ApplicationGenerator

When ApplicationGenerator scans files:

1. Files with a NylRelease are discovered
2. An ArgoCD Application is generated per NylRelease
3. Application metadata comes from the NylRelease

### Singleton Constraint

Only **one** NylRelease is allowed per file. Multiple NylReleases in the same file will cause an error:

```
Error: Multiple NylRelease resources found in file
```

## Examples

### Minimal NylRelease

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: myapp
  namespace: default
```

### With Other Resources

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: myapp
  namespace: production
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: myapp-config
  namespace: production
data:
  environment: production
  version: "1.0.0"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: myapp
  namespace: production
spec:
  replicas: 3
  selector:
    matchLabels:
      app: myapp
  template:
    metadata:
      labels:
        app: myapp
    spec:
      containers:
        - name: myapp
          image: myapp:1.0.0
```

### With HelmChart

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: nginx
  namespace: web
---
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: HelmChart
metadata:
  name: nginx
  namespace: web
spec:
  chart:
    repository: https://charts.bitnami.com/bitnami
    name: nginx
    version: "15.4.4"
  values:
    replicaCount: 2
```

## Use Cases

### ArgoCD Integration

NylRelease is primarily used for ArgoCD integration:

```yaml
# app.yaml in Git repository
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: myapp
  namespace: production
---
# ... other resources ...
```

ArgoCD Application:
```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
spec:
  source:
    plugin:
      name: nyl-v2
      env:
        - name: NYL_RELEASE_NAME
          value: myapp          # From NylRelease.metadata.name
        - name: NYL_RELEASE_NAMESPACE
          value: production     # From NylRelease.metadata.namespace
```

### ApplicationGenerator Discovery

ApplicationGenerator scans for NylRelease resources:

```yaml
# apps.yaml
apiVersion: argocd.nyl.niklasrosenstein.github.com/v1
kind: ApplicationGenerator
spec:
  source:
    path: clusters/default  # Scans this directory
```

For each file in `clusters/default/` with a NylRelease, an Application is generated.

### Metadata Propagation

NylRelease metadata can be used in templates (future enhancement):

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ .release.name }}-config
  namespace: {{ .release.namespace }}
data:
  release_name: {{ .release.name }}
```

## Validation

### Required Fields

- `metadata.name` must be specified
- `metadata.namespace` must be specified

Missing fields cause validation errors:

```
Error: Invalid NylRelease resource: missing field `metadata.name`
```

### Valid Names

Names must follow Kubernetes naming conventions:
- Lowercase alphanumeric characters, `-`, `.`
- Start and end with alphanumeric
- Max 253 characters

## When Not to Use

You don't need NylRelease if:

- Deploying plain Kubernetes manifests without Helm
- Not using ArgoCD integration
- Rendering manifests locally without release context

In these cases, just use regular Kubernetes resources directly.

## See Also

- [ApplicationGenerator](./application-generator.md)
- [ArgoCD Bootstrapping](../../argocd/bootstrapping.md)
- [HelmChart Resource](#) (future documentation)
