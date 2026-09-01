---
title: 'Component System'
---

Nyl's component system lets you map Kubernetes-style resources to Helm charts with a predictable lookup model.

For most teams, the default workflow is:

1. Define local component charts under `components/<apiVersion>/<kind>/`.
2. Reference those components from manifests with:
   - `apiVersion: components.nyl.niklasrosenstein.github.com/v1`
   - `kind: <apiVersion>/<kind>`
3. Put Helm values into `spec`.

## What a Component Resource Does

A Component resource is a compact wrapper around Helm rendering:

- `kind` identifies which chart to render.
- `metadata.name` is used as Helm release name.
- `metadata.namespace` is used as Helm release namespace (defaults to `default` if omitted).
- `spec` is forwarded as Helm values.

Cluster and DeploymentTarget values remain in Nyl's template context and are not
forwarded automatically. Reference them explicitly in `spec` when the chart
accepts the corresponding value:

```yaml
spec:
  environment: "{{ values.environment }}"
```

Example:

```yaml
apiVersion: components.nyl.niklasrosenstein.github.com/v1
kind: example/v1/Nginx
metadata:
  name: web
  namespace: default
spec:
  replicaCount: 2
```

## Choosing Between Component, HelmChart, and Aliases

- Use local Component resources for reusable, repo-owned building blocks.
- Use `HelmChart` for explicit chart definitions in platform-level manifests.
- Use `project.aliases` when you want stable domain `apiVersion/kind` names decoupled from chart source.

See:

- [Authoring Local Components](/nyl/components/authoring-local-components/)
- [Resolution & Lookup Rules](/nyl/components/resolution-and-lookup/)
- [Remote Shortcuts & Aliases](/nyl/components/remote-shortcuts-and-aliases/)
- [Component resource reference](/nyl/reference/resources/component/)
