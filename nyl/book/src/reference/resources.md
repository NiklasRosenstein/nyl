# Resources

Nyl provides Kubernetes-style custom resources for declarative configuration and deployment.

## Resource Types

### Core Resources

- **[NylRelease](./resources/nyl-release.md)**: Defines release metadata (name, namespace) for deployments
- **[HelmChart](./resources/helmchart.md)**: Declarative Helm chart deployment with templating support

### ArgoCD Resources

- **[ApplicationGenerator](./resources/application-generator.md)**: Automatically generates ArgoCD Applications from NylRelease files

### Policy Resources

- **[Kyverno Policies](./kyverno-policies.md)**: Apply Kyverno mutation and validation policies at render time

## Resource Format

All Nyl resources follow Kubernetes resource conventions:

```yaml
apiVersion: <api-version>
kind: <resource-kind>
metadata:
  name: <name>
  namespace: <namespace>  # Optional
spec:
  # Resource-specific fields
```

## API Versions

- `nyl.niklasrosenstein.github.com/v1`: Core Nyl resources (NylRelease, HelmChart)
- `argocd.nyl.niklasrosenstein.github.com/v1`: ArgoCD integration resources (ApplicationGenerator)

## Processing Behavior

### Regular Kubernetes Resources

Regular Kubernetes resources (ConfigMap, Deployment, etc.) are passed through unchanged during `nyl render`.

### Nyl Resources

Nyl resources are processed based on their kind:

- **NylRelease**: Extracted and removed from output (provides metadata only)
- **HelmChart**: Rendered using Helm templating, replaced with rendered manifests
- **ApplicationGenerator**: Processed to generate ArgoCD Applications, removed from output

## Multi-Document Files

Nyl supports YAML multi-document files with `---` separators:

```yaml
apiVersion: nyl.niklasrosenstein.github.com/v1
kind: NylRelease
metadata:
  name: myapp
  namespace: default
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: myapp-config
data:
  key: value
---
apiVersion: v1
kind: Service
metadata:
  name: myapp-svc
spec:
  ports:
    - port: 80
```

Processing:
1. NylRelease is extracted (provides name and namespace)
2. ConfigMap and Service are output as-is

## See Also

- [Configuration](../configuration.md)
- [Commands](../commands/README.md)
