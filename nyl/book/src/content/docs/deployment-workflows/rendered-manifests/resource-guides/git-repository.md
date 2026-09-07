---
title: 'Using GitRepository'
---

See the [GitRepository resource reference](/nyl/reference/resources/gitops.nyl/v1/git-repository/) for the API, example, and field definitions.

## Referencing the repository

`DeploymentTarget.spec.publication.repositoryRef.name` identifies a GitRepository
by its `metadata.name`. This DeploymentTarget publishes to the `deploy`
repository on the `deploy/production` revision:

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata:
  name: production
spec:
  clusterRef:
    name: primary
  argocdRef:
    name: primary
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
    pathPrefix: targets/production
```

The `deploy` GitRepository and the `primary` Cluster and ArgoCDInstance must be
defined in the project.

Both DeploymentTarget publication and ApplicationGroup sources also accept inline
`repository.repoURL` and `repository.publishURL` coordinates. A reference and
an inline repository are mutually exclusive.

## See also

- [DeploymentTarget](/nyl/reference/resources/k8s.gitops.nyl/v1/deployment-target/)
- [ApplicationGroup](/nyl/reference/resources/k8s.gitops.nyl/v1/application-group/)
- [Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/)
