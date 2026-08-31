---
title: 'Schemas'
---

## `nyl.toml` JSON Schema

Generate from CLI:

```bash
nyl generate schema config
```

Committed schema used in published docs:

- [`nyl.schema.json`](./nyl.schema.json)

## Rendered GitOps resource schemas

Each scaffolded resource points at its kind-specific schema:

- [`git-repository.schema.json`](./git-repository.schema.json)
- [`gitops-target.schema.json`](./gitops-target.schema.json)
- [`app-project-definition.schema.json`](./app-project-definition.schema.json)
- [`application-group.schema.json`](./application-group.schema.json)
- [`gitops-resource.schema.json`](./gitops-resource.schema.json) (aggregate)

Regenerate all published artifacts with:

```bash
nyl generate schema all --output-dir book/public/reference/schemas
```
