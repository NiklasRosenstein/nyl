---
title: 'generate'
---

Generate JSON schemas for Nyl configuration and resources.

## Usage

```bash
nyl generate <subcommand> [options]
```

## Subcommands

### schema config

Generate JSON Schema for `nyl.toml` to stdout.

```bash
nyl generate schema config
```

### GitOps schemas

Generate one resource schema, the aggregate resource schema, or every published
schema:

```bash
nyl generate schema resource DeploymentTarget
nyl generate schema resource Cluster
nyl generate schema gitops
nyl generate schema all --output-dir book/public/reference/schemas
```

## See Also

- [Rendered GitOps commands](/nyl/commands/gitops/)
- [Resource reference](/nyl/reference/resources/)
