---
title: 'schema'
---

Generate JSON schemas for Nyl configuration and resources.

```bash
nyl schema config
nyl schema resource DeploymentTarget
nyl schema resource Cluster
nyl schema resource Release
nyl schema resource HelmChart --api-version k8s.nyl/v1
nyl schema resource RemoteManifest
nyl schema resource Component
nyl schema gitops
nyl schema all --output-dir book/public/reference/schemas
```

`config`, `resource`, and `gitops` print one schema to stdout. `all` writes the
complete published schema set to the selected directory.

`--api-version` checks the selected resource's group and version. `Component`
selects the dynamic-kind component envelope schema; it does not require a
literal `kind: Component`.

`all` writes version-qualified resource schemas, stable flat schema aliases,
the GitOps aggregate, the project schema, and `resources.json`. The generated
manifest drives the [resource catalog](/nyl/reference/resources/) and navigation.
Resource purposes, field descriptions, and examples come from the Rust models.
