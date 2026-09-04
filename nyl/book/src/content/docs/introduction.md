---
title: 'Introduction'
---

**Nyl** is a fast Kubernetes manifest generator for teams that want Helm integration, reusable components, cluster-aware configuration, and plain Kubernetes YAML as output.

Nyl can fit into several deployment workflows:

- **Rendered manifest GitOps**: CI renders Nyl inputs into plain YAML and commits or publishes the rendered output for ArgoCD, Flux, or another reconciler.
- **CLI-first operations**: Operators render, diff, and apply directly with `nyl` for bootstrapping, debugging, test clusters, and controlled manual changes.

## What Nyl Provides

- **Helm-backed resources** through `HelmChart` and component shortcuts.
- **Reusable local components** with chart lookup and schema validation.
- **Jinja2-compatible templating** via MiniJinja.
- **Clusters and targets** for deterministic capabilities, deployment values, and publication coordinates.
- **Remote manifests** for including HTTPS-hosted YAML and JSON.
- **Release metadata and revision tracking** through `Release`.
- **Render, diff, and apply commands** for local and CI workflows.
- **A CI rendering image** with Nyl, Helm, Git, SOPS, and Kyverno CLI.

## Choosing a Workflow

Start with the [rendered manifest pattern](/nyl/deployment-workflows/rendered-manifests/) if you want the cluster-side reconciler to consume ordinary Kubernetes manifests and keep Nyl out of the runtime path.

Use [CLI-first workflows](/nyl/deployment-workflows/cli-workflows/) when you need fast local feedback, bootstrap a cluster before GitOps is available, or test a change before committing rendered output.
