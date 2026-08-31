---
title: 'Nyl'
---

Nyl is a fast Kubernetes manifest generator for teams that want Helm integration, reusable components, cluster-aware configuration, and plain Kubernetes YAML as output.

## Choose a Workflow

**Recommended:** The [Rendered Manifest Pattern](/nyl/deployment-workflows/rendered-manifests/) keeps Nyl in CI and lets ArgoCD, Flux, or another reconciler sync ordinary Kubernetes YAML. Start here unless you specifically need controller-side rendering.

- [CLI-First Workflows](/nyl/deployment-workflows/cli-workflows/) use `nyl render`, `nyl diff`, and `nyl apply` for debugging, bootstrapping, testing, and direct operations.
- [ArgoCD CMP Integration](/nyl/argocd/overview/) lets ArgoCD render Nyl inputs directly when you want controller-side rendering, repository credential reuse, or `ApplicationGenerator`.

## Start Building

Begin with [Getting Started](/nyl/getting-started/) to create a project, validate it, and render your first manifests.

For deeper reference material, see [Configuration](/nyl/configuration/), the [Component System](/nyl/components/overview/), and the [Commands](/nyl/commands/).
