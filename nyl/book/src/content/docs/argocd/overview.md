---
title: 'ArgoCD Integration'
---

Nyl works with ArgoCD in two main ways:

- **Rendered manifest GitOps**: CI runs `nyl render` and ArgoCD syncs the rendered YAML with its standard directory support.
- **Config Management Plugin (CMP)**: ArgoCD runs Nyl during sync and receives rendered manifests directly.

Start with the [rendered manifest pattern](/nyl/deployment-workflows/rendered-manifests/) if you want the simplest ArgoCD runtime. Use the CMP integration when ArgoCD should render Nyl inputs itself, reuse ArgoCD repository credentials, or generate Applications with `ApplicationGenerator`.

## Key Features

- **Rendered manifest compatibility**: Sync Nyl output as ordinary Kubernetes YAML.
- **ArgoCD Plugin**: Process Nyl manifests directly within ArgoCD.
- **ApplicationGenerator**: Automatically generate ArgoCD Applications from Nyl releases.
- **Self-Hosting Pattern**: Bootstrap ArgoCD with Nyl using a declarative approach.
- **Repository credential reuse**: Discover ArgoCD repository secrets when Nyl runs inside ArgoCD.

## How It Works

### Rendered Manifests

In the rendered manifest workflow:

1. CI checks out the repository.
2. CI runs `nyl validate` and `nyl render`.
3. CI commits, publishes, or promotes the rendered YAML.
4. ArgoCD syncs the rendered directory with its normal Kubernetes manifest support.

This keeps Nyl out of the ArgoCD repo-server runtime path and makes the rendered YAML reviewable before sync.

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

1. Choose the [rendered manifest pattern](/nyl/deployment-workflows/rendered-manifests/) for a standard ArgoCD directory Application.
2. Choose [Plugin Installation](/nyl/argocd/plugin/) when ArgoCD should render Nyl inputs directly.
3. Follow the [Bootstrapping Guide](/nyl/argocd/bootstrapping/) to set up a self-hosting ArgoCD pattern.
4. Use [ApplicationGenerator](/nyl/argocd/application-generator/) to manage Applications at scale.
5. Review [Best Practices](/nyl/argocd/best-practices/) for production deployments.

## Use Cases

**Rendered Output**: Render Nyl manifests in CI and point ArgoCD at the generated directory.

**Single Application with CMP**: Use the Nyl plugin directly in ArgoCD Applications to render Nyl manifests from Git.

**Multi-Application with CMP**: Use ApplicationGenerator to scan a directory and automatically create Applications for each discovered release.

**Bootstrap Pattern**: Use ApplicationGenerator to have ArgoCD manage itself and all applications declaratively.

**Helm Integration**: Leverage Nyl's HelmChart resource within ArgoCD to deploy Helm charts with full templating support.
