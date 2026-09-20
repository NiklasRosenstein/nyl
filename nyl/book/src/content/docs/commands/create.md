---
title: 'create and delete'
---

Create and delete GitOps control resources in project source. These commands
do not create or delete Kubernetes objects.

## Create resources

```bash
nyl create repository deploy --repo-url https://git.example.com/platform/deploy.git
nyl create cluster primary --context admin@primary
nyl create argocd-instance central
nyl create target production
nyl create app-project workloads
nyl create application-group workloads
```

Nyl locates the nearest `nyl.toml`. When `gitops.yaml` exists at the project
root, the new resource is appended as a YAML document. Otherwise it is written
under the configured `project.gitops_scaffold_path`, which defaults to
`config/<kind>/<name>.yaml`.

`--output` selects an exact file. For an ApplicationGroup, the combination
`--source DIR --colocate` writes `DIR/_application-group.yaml`. Existing files and duplicate
resource identities are never overwritten.

`nyl create cluster` records its local context but does not connect to it. Use
`nyl capture cluster NAME` to refresh stored Kubernetes capabilities.

A generated ApplicationGroup references the project's only AppProjectDefinition
when exactly one exists, and otherwise a project of its own name. It sets no
`destinationNamespace`, so each Release keeps the namespace in its own metadata.

## Create releases

```bash
nyl create release api
nyl create release api --group platform
nyl create release api --namespace api-system --additional-namespaces observability,ingress
```

The Release file is written to the source directory of a matching
ApplicationGroup: `spec.source.path` when the group declares one, the directory
containing a colocated `_application-group.yaml`, and otherwise
`applications/<group-name>`. The directory is created when it does not exist.
`--group` selects the group; it can be omitted when the project declares exactly
one. Groups with a remote or templated `spec.source` need an explicit
`--output`.

When `--group NAME` names a group that does not exist, Nyl offers to declare it
and, with `--create-group`, does so without asking. The new group is appended to
a root `gitops.yaml` or written under `project.gitops_scaffold_path`, and its
Releases live in `applications/NAME`.

`--namespace` sets `metadata.namespace`. It defaults to the group's
`destinationNamespace`, then to the Release name. `--additional-namespaces`
fills `spec.additionalNamespaces` and accepts repeated and comma-separated
values. Namespaces are validated before the file is written, and existing files
are never overwritten. The generated file contains only the Release document;
add the workload manifests as further YAML documents below it.

## Create components

```bash
nyl create component <api-version> <kind>
```

Components are created beneath the first configured
`project.components_search_paths` entry with `Chart.yaml`, `values.yaml`,
`values.schema.json`, and `templates/deployment.yaml`.

## Delete resources

```bash
nyl delete cluster primary
nyl delete application-group workloads --dry-run
```

Deletion removes a dedicated resource file or only the selected document from
a shared YAML stream. It preserves `gitops.yaml` when the last document is
removed and refuses to remove a resource referenced by the remaining project.
Deletion never cascades and does not support component directories.
