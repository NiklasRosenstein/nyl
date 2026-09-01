---
title: 'Resources'
---

Nyl provides Kubernetes-style custom resources for declarative configuration and deployment.

## Resource Types

### Core Resources

- **[Component](/nyl/reference/resources/component/)**: Compact chart-backed resource using dynamic `kind` lookup
- **[HelmChart](/nyl/reference/resources/helmchart/)**: Declarative Helm chart deployment with templating support
- **[RemoteManifest](/nyl/reference/resources/remote-manifest/)**: Fetch and include manifests from a remote HTTPS URL

### ArgoCD Resources

- **[ApplicationGenerator](/nyl/reference/resources/application-generator/)**: Automatically generates ArgoCD Applications from Release files

### Rendered GitOps Resources

- **[Release](/nyl/reference/resources/gitops/release/)**: Defines one rendered deployment unit and its namespace scope
- **[GitRepository](/nyl/reference/resources/gitops/git-repository/)**: Names credential-free Git read and publication coordinates
- **[Cluster](/nyl/reference/resources/gitops/cluster/)**: Defines a concrete destination and deterministic Kubernetes capabilities
- **[GitOpsTarget](/nyl/reference/resources/gitops/gitops-target/)**: Binds a Cluster to values and publication coordinates
- **[AppProjectDefinition](/nyl/reference/resources/gitops/app-project-definition/)**: Defines a rendered or external Argo CD AppProject contract
- **[ApplicationGroup](/nyl/reference/resources/gitops/application-group/)**: Selects releases and owns generated Application and Namespace policy

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

- `nyl.niklasrosenstein.github.com/v1`: Core rendering resources (`HelmChart`, `RemoteManifest`)
- `argocd.nyl.niklasrosenstein.github.com/v1`: ArgoCD integration resources (ApplicationGenerator)
- `gitops.nyl/v1`: Release metadata and rendered GitOps control resources (`Release`, `GitRepository`, `Cluster`, `GitOpsTarget`, `AppProjectDefinition`, `ApplicationGroup`)
- `components.nyl.niklasrosenstein.github.com/v1`: Component resources (dynamic `kind` path/shortcut)

## Processing Behavior

### Regular Kubernetes Resources

Regular Kubernetes resources (ConfigMap, Deployment, etc.) are passed through unchanged during `nyl render`.

### Nyl Resources

Nyl resources are processed based on their kind:

- **Release**: Extracted and removed from output (provides metadata only)
- **Component**: Resolved to a chart reference and rendered via Helm, replaced with rendered manifests
- **HelmChart**: Rendered using Helm templating, replaced with rendered manifests
- **RemoteManifest**: Fetched via HTTPS and parsed into documents, then processed recursively
- **ApplicationGenerator**: Processed to generate ArgoCD Applications, removed from output
- **Rendered GitOps resources**: Discovered as compiler configuration for `render-tree`, `diff-tree`, and `publish-tree`; not emitted as workload manifests

## Multi-Document Files

Nyl supports YAML multi-document files with `---` separators:

```yaml
apiVersion: gitops.nyl/v1
kind: Release
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
1. Release is extracted (provides name and namespace)
2. ConfigMap and Service are output as-is

## See Also

- [Configuration](/nyl/configuration/)
- [Commands](../commands/)
