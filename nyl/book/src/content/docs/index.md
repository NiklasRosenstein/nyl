---
title: 'Nyl'
---

Nyl turns configuration in Git into deployed systems. It renders Kubernetes
manifests from Helm charts, components, and templates, and applies them
directly or publishes them for Argo CD, Flux, or another GitOps reconciler.
Every command acts on that declared configuration and takes the same path,
whether you run it on a laptop or in CI. The
[introduction](/nyl/start/introduction/) explains each way to use it.

## Start building

Begin with [Getting Started](/nyl/start/getting-started/) to create a project, validate it, and render your first manifests.

## Find your way

- **[Kubernetes](/nyl/kubernetes/pipeline/)**: render, validate, and apply manifests.
- **[Rendered GitOps](/nyl/rendered-gitops/)**: publish rendered manifests to Git for a reconciler to sync.
- **[Reference](/nyl/reference/cli/)**: every command, YAML resource, and `nyl.toml` setting.
