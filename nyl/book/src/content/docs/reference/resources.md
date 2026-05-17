---
title: 'Resources'
---

Nyl provides Kubernetes-style custom resources for declarative configuration and deployment.

## Resource Types

### Core Resources

- **[NylRelease](/nyl/reference/resources/nyl-release/)**: Defines release metadata (name, namespace) for deployments
- **[Component](/nyl/reference/resources/component/)**: Compact chart-backed resource using dynamic `kind` lookup
- **[HelmChart](/nyl/reference/resources/helmchart/)**: Declarative Helm chart deployment with templating support
- **[RemoteManifest](/nyl/reference/resources/remote-manifest/)**: Fetch and include manifests from a remote HTTPS URL

### ArgoCD Resources

- **[ApplicationGenerator](/nyl/reference/resources/application-generator/)**: Automatically generates ArgoCD Applications from NylRelease files

### Policy Resources

- **[Kyverno Policies](/nyl/reference/kyverno-policies/)**: Apply Kyverno mutation and validation policies at render time

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
  - Includes: `NylRelease`, `HelmChart`, `RemoteManifest`
- `argocd.nyl.niklasrosenstein.github.com/v1`: ArgoCD integration resources (ApplicationGenerator)
- `components.nyl.niklasrosenstein.github.com/v1`: Component resources (dynamic `kind` path/shortcut)

## Processing Behavior

### Regular Kubernetes Resources

Regular Kubernetes resources (ConfigMap, Deployment, etc.) are passed through unchanged during `nyl render`.

### Nyl Resources

Nyl resources are processed based on their kind:

- **NylRelease**: Extracted and removed from output (provides metadata only)
- **Component**: Resolved to a chart reference and rendered via Helm, replaced with rendered manifests
- **HelmChart**: Rendered using Helm templating, replaced with rendered manifests
- **RemoteManifest**: Fetched via HTTPS and parsed into documents, then processed recursively
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

- [Configuration](/nyl/configuration/)
- [Commands](../commands/)
