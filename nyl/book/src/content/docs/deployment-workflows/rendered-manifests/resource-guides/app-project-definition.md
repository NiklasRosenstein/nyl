---
title: 'Using AppProjectDefinition'
---

See the [AppProjectDefinition resource reference](/nyl/reference/resources/k8s.gitops.nyl/v1/app-project-definition/) for the API, example, and field definitions.

## Target-dependent policy

`spec` may use Nyl structural templating so destinations, source repositories,
or other AppProject policy can vary by target. The resource's `apiVersion`,
`kind`, and `metadata.name` remain static. Rendering fails if an applicable
ApplicationGroup references a project omitted for that target.

Argo CD enforces the resulting AppProject policy at reconciliation time. Keep
the definition under platform-owner review; see
[Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/).

## See also

- [ApplicationGroup](/nyl/reference/resources/k8s.gitops.nyl/v1/application-group/)
- [DeploymentTarget](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/)
