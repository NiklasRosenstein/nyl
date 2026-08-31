---
title: 'GitRepository'
---

`GitRepository` assigns a stable local name to credential-free Git coordinates.
Targets use it for publication and ApplicationGroups can use it for remote
sources.

## Example

```yaml
apiVersion: gitops.nyl/v1
kind: GitRepository
metadata:
  name: deploy
spec:
  repoURL: https://git.example.com/platform/deploy.git
  publishURL: git@git.example.com:platform/deploy.git
```

## Fields

| Field | Required | Description |
| --- | --- | --- |
| `metadata.name` | Yes | Project-local repository identity. |
| `metadata.labels` | No | Labels for organization and tooling. |
| `spec.repoURL` | Yes | Credential-free URL written to generated Argo CD Applications and used for reads. |
| `spec.publishURL` | No | Distinct URL used for publication writes. Defaults to `repoURL`. |

`repoURL` and `publishURL` must be non-empty static values. HTTP URLs with user
information are rejected. Keep credentials in Git configuration, CI secrets,
or Argo CD repository credentials rather than resource YAML.

## Referencing the repository

A target normally references the local identity:

```yaml
spec:
  publication:
    repositoryRef:
      name: deploy
    revision: deploy/production
```

Both GitOpsTarget publication and ApplicationGroup sources also accept inline
`repository.repoURL` and `repository.publishURL` coordinates. A reference and
an inline repository are mutually exclusive.

## See also

- [GitOpsTarget](/nyl/reference/resources/gitops/gitops-target/)
- [ApplicationGroup](/nyl/reference/resources/gitops/application-group/)
- [Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/)
