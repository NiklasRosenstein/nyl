---
title: 'Rendered Manifest GitOps'
---

Rendered manifest GitOps is the recommended Nyl deployment model. Nyl compiles
trusted source configuration into ordinary Kubernetes YAML in a deployment Git
revision. Argo CD reads plain recursive directories and does not need the Nyl
CMP.

## How the model fits together

Five Kubernetes-shaped configuration resources describe the deployment:

1. A [`GitRepository`](/nyl/reference/resources/gitops/git-repository/) names
   credential-free read and write coordinates.
2. A [`Cluster`](/nyl/reference/resources/gitops/cluster/) records one concrete
   destination and the Kubernetes capabilities used for offline rendering.
3. A [`GitOpsTarget`](/nyl/reference/resources/gitops/gitops-target/) binds that
   Cluster to deployment values and a publication repository, revision, and
   path prefix.
4. An [`AppProjectDefinition`](/nyl/reference/resources/gitops/app-project-definition/)
   provides the Argo CD project contract.
5. An [`ApplicationGroup`](/nyl/reference/resources/gitops/application-group/)
   selects targets and source releases, then defines the generated Application
   and Namespace policy.

These resources are compiler inputs. They are not installed in Kubernetes.
`nyl render-tree` produces workload manifests, managed Namespaces, Argo CD
AppProjects and Applications, and an ownership index. `nyl publish-tree` can
commit that tree to the target revision.

## Quick start

Create a project and the control resources:

```bash
nyl new project platform
nyl new gitops repository deploy
nyl new gitops cluster primary
nyl new gitops target production
nyl new gitops project workloads
nyl new gitops application-group workloads
```

Then validate and render one target:

```bash
nyl validate gitops
nyl target list
nyl render-tree --target production --output-dir deploy-worktree
```

Argo CD points at the generated catalog and workload directories on the
target's publication revision. Generated Applications use ordinary Git
directory sources with recursive discovery enabled.

## Guides

- [Project structure and discovery](/nyl/deployment-workflows/rendered-manifests/project-structure/)
  explains the conventional layout, colocated groups, and remote sources.
- [Targets and cluster variation](/nyl/deployment-workflows/rendered-manifests/targets-and-clusters/)
  covers multiple environments, clusters, publication models, values, and
  conditional rendering.
- [Rendering, diffing, and publishing](/nyl/deployment-workflows/rendered-manifests/rendering-and-publishing/)
  describes the generated tree and CI commands.
- [Trust and admission boundaries](/nyl/deployment-workflows/rendered-manifests/security/)
  identifies what Nyl validates and what the Git forge, Argo CD, and Kubernetes
  must enforce.
- [Rendered GitOps resource reference](/nyl/reference/resources/gitops/)
  documents every configuration field by resource kind.

For direct cluster operations and debugging, see
[CLI-first workflows](/nyl/deployment-workflows/cli-workflows/). For runtime
rendering inside Argo CD, see the [Argo CD CMP integration](/nyl/argocd/plugin/).
