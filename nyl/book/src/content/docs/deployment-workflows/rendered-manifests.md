---
title: 'Rendered Manifest Pattern'
---

The rendered manifest pattern is the recommended Nyl deployment model. Nyl
compiles trusted source configuration into ordinary Kubernetes YAML in a
deployment Git revision. Argo CD reads plain recursive directories and does not
run Nyl in the reconciliation path.

## How the model fits together

Kubernetes-shaped configuration resources describe the deployment:

1. A [`GitRepository`](/nyl/reference/resources/gitops/git-repository/) names
   credential-free read and write coordinates.
2. A [`Cluster`](/nyl/reference/resources/gitops/cluster/) records one concrete
   destination and the Kubernetes capabilities used for offline rendering.
3. A [`DeploymentTarget`](/nyl/reference/resources/gitops/deployment-target/) binds that
   Cluster to deployment values and a publication repository, revision, and
   path prefix.
4. An optional [`ArgoCDInstance`](/nyl/reference/resources/gitops/argocd-instance/)
   separates the Argo CD control plane from workload destinations and owns
   parent-catalog defaults.
5. An [`ApplicationGroup`](/nyl/reference/resources/gitops/application-group/)
   declares source releases and generated Application and Namespace policy. It
   references an AppProjectDefinition or uses `projectTemplate` to generate one.

These resources are compiler inputs. They are not installed in Kubernetes.
`nyl render-tree` produces workload manifests, managed Namespaces, Argo CD
AppProjects and Applications, and an ownership index. `nyl publish-tree` can
commit that tree to the target revision.

## Quick start

Initialize a Git repository with one cluster, deployment target, AppProject, and
ApplicationGroup:

```bash
mkdir platform && cd platform
git init
git remote add origin https://git.example.com/platform/deploy.git
nyl init gitops --cluster-name production --context admin@production
```

The wizard detects the Git remote and current kube context, writes a compact
`gitops.yaml`, creates a minimal `nyl.toml` when needed, and creates the
`applications/` source directory. See [`nyl init gitops`](/nyl/commands/init/)
for fully non-interactive flags and stdout mode.

Then validate and render one target:

```bash
nyl validate gitops
nyl target list
nyl render-tree --target production --output-dir deploy-worktree
```

Seed the generated `<target>-catalog` Application once after the first publish.
It then recursively manages the generated catalog, including itself. Generated
Applications use ordinary Git directory sources with recursive discovery.

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
[CLI-first workflows](/nyl/deployment-workflows/cli-workflows/).
