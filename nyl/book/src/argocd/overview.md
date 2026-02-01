# ArgoCD Integration

Nyl provides first-class support for ArgoCD through a custom configuration management plugin and the ApplicationGenerator resource. This integration enables GitOps-style deployments where ArgoCD can render Nyl manifests directly from Git repositories.

## Key Features

- **ArgoCD Plugin**: Process Nyl manifests directly within ArgoCD
- **ApplicationGenerator**: Automatically generate ArgoCD Applications from Nyl releases
- **Self-Hosting Pattern**: Bootstrap ArgoCD with Nyl using a declarative approach
- **Live Diffing**: See differences between Git state and cluster state
- **Automated Sync**: Optionally enable automatic synchronization and pruning

## How It Works

### The Plugin

Nyl provides an ArgoCD configuration management plugin that acts as a bridge between ArgoCD and Nyl. When ArgoCD syncs an Application that uses the Nyl plugin:

1. ArgoCD clones the Git repository
2. The Nyl plugin is invoked to render manifests
3. Nyl processes the YAML files (HelmCharts, NylRelease, etc.)
4. Rendered Kubernetes manifests are returned to ArgoCD
5. ArgoCD applies the manifests to the cluster

### ApplicationGenerator

The ApplicationGenerator resource enables automatic discovery and generation of ArgoCD Applications. When you use `nyl render` on a file containing an ApplicationGenerator:

1. Nyl scans the configured directory for YAML files
2. Each file with a NylRelease is discovered
3. An ArgoCD Application is generated for each NylRelease
4. The ApplicationGenerator is replaced with the generated Applications

This pattern is particularly useful for:
- Managing multiple applications in a single repository
- Bootstrapping ArgoCD to manage itself
- Multi-cluster deployments
- Directory-based organization

## Getting Started

1. [Install the ArgoCD Plugin](./plugin.md) in your ArgoCD installation
2. Follow the [Bootstrapping Guide](./bootstrapping.md) to set up Nyl with ArgoCD
3. Use [ApplicationGenerator](./application-generator.md) to manage applications at scale
4. Review [Best Practices](./best-practices.md) for production deployments

## Use Cases

**Single Application**: Use the Nyl plugin directly in ArgoCD Applications to render Nyl manifests from Git.

**Multi-Application**: Use ApplicationGenerator to scan a directory and automatically create Applications for each discovered release.

**Bootstrap Pattern**: Use ApplicationGenerator to have ArgoCD manage itself and all applications declaratively.

**Helm Integration**: Leverage Nyl's HelmChart resource within ArgoCD to deploy Helm charts with full templating support.
