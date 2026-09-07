---
title: 'schema'
---

Generate JSON schemas for Nyl configuration and resources.

```bash
nyl schema config
nyl schema resource DeploymentTarget
nyl schema resource Cluster
nyl schema resource Release
nyl schema resource HelmChart
nyl schema resource RemoteManifest
nyl schema resource Component
nyl schema gitops
nyl schema all --output-dir book/public/reference/schemas
```

`config`, `resource`, and `gitops` print one schema to stdout. `all` writes the
complete published schema set to the selected directory.

Each resource kind currently belongs to one API version, so the kind alone
selects its schema. `--api-version` is optional: it checks that the selected
resource uses the expected group and version, and reports an error on a mismatch.
For example, this produces the same schema as `nyl schema resource HelmChart`:

```bash
nyl schema resource HelmChart --api-version k8s.nyl/v1
```

`Component` selects the dynamic-kind component envelope schema; it does not require a
literal `kind: Component`.

`all` writes version-qualified resource schemas, stable flat schema aliases,
the GitOps aggregate, the project schema, and `resources.json`. The generated
manifest drives the [resource catalog](/nyl/reference/resources/) and navigation.
Resource purposes, field descriptions, and examples come from the Rust models.
