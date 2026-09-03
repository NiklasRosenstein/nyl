---
title: 'schema'
---

Generate JSON schemas for Nyl configuration and resources.

```bash
nyl schema config
nyl schema resource DeploymentTarget
nyl schema resource Cluster
nyl schema resource Release
nyl schema gitops
nyl schema all --output-dir book/public/reference/schemas
```

`config`, `resource`, and `gitops` print one schema to stdout. `all` writes the
complete published schema set to the selected directory.
