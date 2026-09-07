---
title: 'Using GitRepository'
---

See the [GitRepository resource reference](/nyl/reference/resources/gitops.nyl/v1/git-repository/) for the API, example, and field definitions.

## Referencing the repository

A target normally references the local identity:

```yaml
spec:
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
```

Both DeploymentTarget publication and ApplicationGroup sources also accept inline
`repository.repoURL` and `repository.publishURL` coordinates. A reference and
an inline repository are mutually exclusive.

## See also

- [DeploymentTarget](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/)
- [ApplicationGroup](/nyl/reference/resources/k8s.gitops.nyl/v1/application-group/)
- [Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/)
