---
title: 'AppProjectDefinition'
---

`AppProjectDefinition` assigns a stable local identity to an Argo CD AppProject
contract. ApplicationGroups refer to the local identity while generated
Applications use `spec.manifest.metadata.name` as their Argo CD project.

The wrapper records Nyl-side ownership and policy that an ordinary AppProject
does not carry: `Rendered` versus `External` management, a stable local
`projectRef`, and target-specific structural templating. Argo CD still enforces
the contained AppProject policy. A raw AppProject manifest does not satisfy an
ApplicationGroup `projectRef`. Use `ApplicationGroup.spec.projectTemplate` when
the constrained generated shape is sufficient.

## Rendered project example

```yaml
apiVersion: gitops.nyl/v1
kind: AppProjectDefinition
metadata:
  name: workloads
spec:
  management: Rendered
  sourceRepositoryRefs:
    - name: deploy
  manifest:
    apiVersion: argoproj.io/v1alpha1
    kind: AppProject
    metadata:
      name: workloads
      namespace: argocd
    spec:
      destinations:
        - server: https://kubernetes.default.svc
          namespace: '*'
```

## Fields

| Field | Required | Description |
| --- | --- | --- |
| `metadata.name` | Yes | Project-local identity referenced by groups. |
| `metadata.labels` | No | Labels for organization and tooling. |
| `spec.management` | Yes | `Rendered` or `External`. |
| `spec.sourceRepositoryRefs[].name` | No | Local GitRepository identities whose `repoURL` values are added to the rendered AppProject's `sourceRepos`. Valid only for `Rendered` projects. |
| `spec.manifest` | Yes | An `argoproj.io/v1alpha1` `AppProject` object with a valid name and object-valued `spec`. |

`Rendered` writes the AppProject manifest to
`_nyl/catalog/projects/<metadata.name>.yaml` when an applicable group uses it.
Nyl sets the rendered object's namespace to the effective ArgoCDInstance
namespace so the project and its Applications belong to the same control
plane.
Referenced repository URLs are appended to literal `manifest.spec.sourceRepos`
in declaration order and deduplicated. `publishURL` is a writer coordinate and
is never granted to Argo CD.
`External` uses the manifest as a contract and project-name source but does not
publish it. The external AppProject must already be managed through another
administrator-approved path.

## Target-dependent policy

`spec` may use Nyl structural templating so destinations, source repositories,
or other AppProject policy can vary by target. The resource's `apiVersion`,
`kind`, and `metadata.name` remain static. Rendering fails if an applicable
ApplicationGroup references a project omitted for that target.

Argo CD enforces the resulting AppProject policy at reconciliation time. Keep
the definition under platform-owner review; see
[Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/).

## See also

- [ApplicationGroup](/nyl/reference/resources/gitops/application-group/)
- [DeploymentTarget](/nyl/reference/resources/gitops/deployment-target/)
