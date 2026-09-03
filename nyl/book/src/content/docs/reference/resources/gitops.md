---
title: 'Rendered GitOps Resources'
---

Rendered GitOps resources are Kubernetes-shaped compiler inputs with API
version `gitops.nyl/v1`. They describe repositories,
cluster capabilities, publication targets, Argo CD projects, and application
policy. Nyl discovers them from project YAML files; they are not installed in a
cluster.

## Resource kinds

- [`GitRepository`](/nyl/reference/resources/gitops/git-repository/) defines a
  reusable, credential-free repository identity.
- [`Cluster`](/nyl/reference/resources/gitops/cluster/) defines a concrete Argo
  CD destination and deterministic Kubernetes capabilities.
- [`ArgoCDInstance`](/nyl/reference/resources/gitops/argocd-instance/) defines
  an Argo CD control plane and catalog defaults.
- [`DeploymentTarget`](/nyl/reference/resources/gitops/deployment-target/) defines one
  independently rendered and published deployment slice.
- [`AppProjectDefinition`](/nyl/reference/resources/gitops/app-project-definition/)
  defines a local identity for a rendered or externally managed Argo CD
  AppProject.
- [`ApplicationGroup`](/nyl/reference/resources/gitops/application-group/)
  declares release sources and owns generated Application, AppProject, and Namespace
  policy.
- [`Release`](/nyl/reference/resources/gitops/release/) defines one deployment
  unit, its namespace scope, included manifests, and approved Application
  customization.

## Common envelope

```yaml
apiVersion: gitops.nyl/v1
kind: <resource-kind>
metadata:
  name: <local-name>
  labels: {}
spec: {}
```

`metadata.name` is a Kubernetes DNS subdomain and forms a project-local identity
with `kind`. `metadata.labels` is an optional string map. The API version, kind,
and name must remain static so Nyl can discover resources without evaluating
templates.

Files can live anywhere visible to Git discovery. The
[recommended project structure](/nyl/deployment-workflows/rendered-manifests/project-structure/)
groups them under `config/` for readability.

## Validation and schemas

Validate the complete reference graph with:

```bash
nyl validate
```

Kind-specific JSON schemas are available from the
[Nyl Resource Schemas](/nyl/extras/nyl-resource-schemas/). Scaffolded resources include the
appropriate YAML language-server schema URL.

See the [Rendered Manifest Pattern](/nyl/deployment-workflows/rendered-manifests/)
for the operational workflow.
