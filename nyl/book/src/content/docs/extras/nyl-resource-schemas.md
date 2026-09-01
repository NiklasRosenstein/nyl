---
title: 'Nyl Resource Schemas'
---

## `nyl.toml` JSON Schema

Generate from CLI:

```bash
nyl generate schema config
```

Committed schema used in published docs:

- [`nyl.schema.json`](/nyl/reference/schemas/nyl.schema.json)

## Rendered GitOps resource schemas

Each scaffolded resource points at its kind-specific schema:

- [`git-repository.schema.json`](/nyl/reference/schemas/git-repository.schema.json)
- [`cluster.schema.json`](/nyl/reference/schemas/cluster.schema.json)
- [`argocd-instance.schema.json`](/nyl/reference/schemas/argocd-instance.schema.json)
- [`deployment-target.schema.json`](/nyl/reference/schemas/deployment-target.schema.json)
- [`app-project-definition.schema.json`](/nyl/reference/schemas/app-project-definition.schema.json)
- [`application-group.schema.json`](/nyl/reference/schemas/application-group.schema.json)
- [`release.schema.json`](/nyl/reference/schemas/release.schema.json)
- [`gitops-resource.schema.json`](/nyl/reference/schemas/gitops-resource.schema.json) (aggregate)

Regenerate all published artifacts with:

```bash
nyl generate schema all --output-dir book/public/reference/schemas
```
