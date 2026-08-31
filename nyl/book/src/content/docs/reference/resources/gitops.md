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
- [`GitOpsTarget`](/nyl/reference/resources/gitops/gitops-target/) defines one
  independently rendered and published deployment slice.
- [`AppProjectDefinition`](/nyl/reference/resources/gitops/app-project-definition/)
  defines a local identity for a rendered or externally managed Argo CD
  AppProject.
- [`ApplicationGroup`](/nyl/reference/resources/gitops/application-group/)
  selects releases and targets and owns generated Application and Namespace
  policy.

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
nyl validate gitops
```

Kind-specific JSON schemas are available from the
[schema reference](/nyl/reference/schemas/). Scaffolded resources include the
appropriate YAML language-server schema URL.

See the [Rendered Manifest Pattern](/nyl/deployment-workflows/rendered-manifests/)
for the operational workflow.
