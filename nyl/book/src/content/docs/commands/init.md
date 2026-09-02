---
title: 'init'
---

`nyl init gitops [DIR]` initializes the compact form of a rendered-manifest
GitOps project. `DIR` defaults to the current directory and must be inside a
Git worktree.

```bash
nyl init gitops
```

When attached to a terminal, the command proposes values detected from the Git
`origin` remote and the current kubeconfig context. It creates:

- `nyl.toml` when the project has none
- one `gitops.yaml` containing `GitRepository`, `Cluster`,
  `DeploymentTarget`, `AppProjectDefinition`, and `ApplicationGroup`
- the ApplicationGroup source directory, defaulting to `applications/`

The ApplicationGroup is part of the simple configuration unless
`--skip-applications` is set. Release metadata determines each workload's
destination namespace; the generated group does not impose a
`destinationNamespace`.

The default AppProject allows the `default` namespace and the core `Namespace`
kind. Repository access uses `sourceRepositoryRefs`, so Argo CD receives the
credential-free `GitRepository.spec.repoURL`, never its publication URL.

## Non-interactive use

`--yes` accepts detected values and defaults without prompting. Important
options include:

```text
--output <PATH|->
--repository-name <NAME>
--repo-url <URL>
--publish-url <URL>
--cluster-name <NAME>
--context <CONTEXT> | --no-context
--destination-server <URL> | --destination-name <NAME>
--update-cluster | --no-update-cluster
--target-name <NAME>
--revision <REVISION>
--path-prefix <PATH>
--argocd-namespace <NAMESPACE>
--project-name <NAME>
--allow-namespace <NAMESPACE>
--allow-cluster-resource <GROUP/KIND>
--applications-path <PATH>
--applications-name <NAME>
--skip-applications
```

Repeat the two `--allow-*` options to build a larger least-privilege AppProject.
Use `core/Namespace` for a core API resource. Nyl warns when `*` grants access
to every namespace.

An interactive run offers to fetch the cluster's Kubernetes version and API
versions when its context exists. A non-interactive run performs that network
operation only with `--update-cluster`.

`--output -` prints only the multi-document configuration to stdout and does
not create `nyl.toml`, `gitops.yaml`, or the applications directory. Existing
configuration files are never overwritten.

## Derived defaults

The initial Cluster and DeploymentTarget share a name unless `--target-name` is
set. In that 1:1 form the DeploymentTarget omits `clusterRef`; Nyl resolves the
same-named Cluster. The target also omits `publication.pathPrefix`, which makes
the target name its rendered-tree prefix. Pass an explicit empty
`--path-prefix ''` to publish at the repository root.
