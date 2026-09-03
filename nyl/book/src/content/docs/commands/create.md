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
`nyl update cluster NAME` to refresh stored Kubernetes capabilities.

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
