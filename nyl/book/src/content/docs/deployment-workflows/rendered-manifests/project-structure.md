---
title: 'Project Structure and Discovery'
---

Rendered GitOps resources are discovered project-wide. Tracked files and
non-ignored untracked YAML files are eligible, so directory names are
conventions rather than mandatory lookup paths.

## Recommended layout

The smallest useful repository keeps all control resources in one file:

```text
nyl.toml
gitops.yaml
applications/
components/
```

Create it with `nyl init`. GitOps resource discovery is independent of
the file name and directory layout, so the configuration can be split as it
grows. A conventional split layout is:

```text
nyl.toml
config/
  repositories/
    deploy.yaml
    workloads.yaml
  clusters/
    primary.yaml
  argocd-instances/
    central.yaml
  targets/
    production.yaml
  projects/
    workloads.yaml
  application-groups/
    workloads.yaml
applications/
  app1.yaml
  workloads/
    app2.yaml
components/
```

Set `project.gitops_scaffold_path` in `nyl.toml` when generated control
resources should live somewhere other than `config/`. This setting changes only
scaffold destinations; it does not restrict discovery.

Create additional resources individually with:

```bash
nyl create repository deploy --repo-url https://git.example.com/platform/deploy.git
nyl create cluster primary --context admin@primary
nyl create argocd-instance central
nyl create target production
nyl create app-project workloads
nyl create application-group workloads
```

## Application source paths

A centrally stored
[`ApplicationGroup`](/nyl/reference/resources/gitops/application-group/) with
no explicit source derives `applications/<group-name>`. An explicit
`spec.source.path` can select any other project-relative directory.

An ApplicationGroup can instead be colocated with its source:

```bash
nyl create application-group workloads \
  --source applications/workloads \
  --colocate
```

This writes `applications/workloads/_application-group.yaml`. The group derives
its source directory from that location, so the file does not need to repeat
the path.

## Remote sources

An ApplicationGroup can select releases from another Git repository. A remote
source records both a human-readable mutable `revision` and an authoritative
full `commit` lock. Update locks with:

```bash
nyl update source-locks workloads
nyl update source-locks --check
```

Central renderer mode applies the platform project's `nyl.toml` and component
configuration to the remote source. Remote renderer mode loads configuration
from `spec.source.rendererConfig.projectPath` in the remote repository. See the
[ApplicationGroup reference](/nyl/reference/resources/gitops/application-group/#source-selection)
for the complete source contract and the
[security guide](/nyl/deployment-workflows/rendered-manifests/security/#remote-source-isolation)
for its trust boundary.

## Discovery constraints

The `apiVersion`, `kind`, and `metadata.name` of every GitOps resource are a
static discovery envelope. `ApplicationGroup.spec` and
`AppProjectDefinition.spec` may use target-dependent structural templating, but
the envelope cannot. A source-lock update also requires the ApplicationGroup's
source coordinates and lock to remain statically parseable.

Use `nyl validate` to detect duplicate identities, invalid references,
overlapping publication paths, unsafe paths, and unsupported templating before
rendering.

## Next steps

- [Targets and cluster variation](/nyl/deployment-workflows/rendered-manifests/targets-and-clusters/)
- [Rendering, diffing, and publishing](/nyl/deployment-workflows/rendered-manifests/rendering-and-publishing/)
- [Rendered GitOps resource reference](/nyl/reference/resources/gitops/)
