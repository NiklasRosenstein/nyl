---
title: 'init'
---

`nyl init [DIR]` initializes the compact form of a rendered-manifest
GitOps project. `DIR` defaults to the current directory and must be inside a
Git worktree.

```bash
nyl init
```

Use `nyl init DIR --minimal` to create only `nyl.toml` and the conventional
project directories. Minimal initialization can create a project outside Git;
GitOps-specific options cannot be combined with `--minimal`.

`--vendor <disabled|preferred|required>` records a
[remote artifact vendoring](/nyl/configuration/#remote-artifact-vendoring)
policy in the generated `nyl.toml`. It applies to both forms of initialization
and writes only the `[vendor] mode` setting; `path` and `lfs_threshold_bytes`
keep their defaults. Because initialization never rewrites an existing
`nyl.toml`, the option is rejected when the project already has one that
declares a different policy, and when `--output -` writes no project
configuration at all.

When attached to a terminal, the command proposes values detected from the Git
`origin` remote and the current kubeconfig context. It creates:

- `nyl.toml` when the project has none
- one `gitops.yaml` containing `GitRepository`, `Cluster`,
  `DeploymentTarget`, and `ApplicationGroup`
- the ApplicationGroup source directory, defaulting to `applications/`

The ApplicationGroup is part of the simple configuration unless
`--skip-applications` is set. Release metadata determines each workload's
destination namespace; the generated group does not impose a
`destinationNamespace`.

No project resource is written. The group keeps its
[implied AppProject](/nyl/deployment-workflows/rendered-manifests/resource-guides/application-group/#project-assignment):
named after the group, confined to the target cluster and the publication
repository, and permissive about namespaces and cluster-scoped resources. Argo
CD receives the credential-free `GitRepository.spec.repoURL`, never the
publication URL.

`--project-name`, `--allow-namespace`, and `--allow-cluster-resource` narrow
that project by writing an explicit `spec.projectTemplate`. Each dimension the
options leave out stays permissive, so `--allow-namespace apps` restricts
namespaces while cluster-scoped resources remain open.

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
--capture-cluster | --no-capture-cluster
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
--vendor <MODE>
```

Repeat the two `--allow-*` options to build a larger least-privilege AppProject.
Use `core/Namespace` for a core API resource.

An interactive run offers to fetch the cluster's Kubernetes version and API
versions when its context exists. A non-interactive run performs that network
operation only with `--capture-cluster`.

`--output -` prints only the multi-document configuration to stdout and does
not create `nyl.toml`, `gitops.yaml`, or the applications directory. Existing
configuration files are never overwritten.

## Derived defaults

The initial Cluster and DeploymentTarget share a name unless `--target-name` is
set. In that 1:1 form the DeploymentTarget omits `clusterRef`; Nyl resolves the
same-named Cluster. The target also omits `publication.pathPrefix`, which makes
the target name its rendered-tree prefix. Pass an explicit empty
`--path-prefix ''` to publish at the repository root.
