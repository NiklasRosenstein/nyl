---
title: 'new'
---

Create projects, components, and rendered GitOps control resources.

## Synopsis

```bash
nyl new project <dir>
nyl new component <api-version> <kind>
nyl new resource <KIND> <name>
nyl new gitops <repository|cluster|target|project|application-group> <name>
```

## `nyl new project`

Creates:

```text
<dir>/
├── nyl.toml
├── applications/
├── components/
└── config/
    ├── repositories/
    ├── clusters/
    ├── targets/
    ├── projects/
    └── application-groups/
```

Generated `nyl.toml`:

```toml
[project]
components_search_paths = ["components"]
helm_chart_search_paths = ["."]
gitops_scaffold_path = "config"
```

## `nyl new component`

Creates component chart files under:

```text
components/<api-version>/<kind>/
├── Chart.yaml
├── values.yaml
├── values.schema.json
└── templates/deployment.yaml
```

## GitOps resources

The generic and kind-specific forms use the same scaffold registry:

```bash
nyl new resource GitRepository deploy
nyl new gitops repository deploy
nyl new resource Cluster primary
nyl new gitops cluster primary --context admin@primary
```

Every file includes its published YAML language server schema URL. Use
`--output` for an exact path, `--source` to set an ApplicationGroup source, or
`--source ... --colocate` to create `_application-group.yaml` inside the source
directory. Existing files are never overwritten.

`nyl new gitops cluster` writes `--context` to `spec.live.context`. Without the
option, the Cluster name is the implied context. Nyl warns when the selected
context is absent from the local kubeconfig. When it exists and stdin is a
terminal, Nyl offers to run `nyl cluster update` immediately; piped and CI
invocations never prompt or contact the cluster.

Created paths are printed relative to the current directory when that requires
at most two leading `..` segments. More distant paths are printed as absolute
paths.
