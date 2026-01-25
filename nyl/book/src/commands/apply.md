# apply

> **Status**: Phase 4 (Not yet implemented)

Apply rendered manifests to the Kubernetes cluster.

## Synopsis

```bash
nyl apply [options]
```

## Description

The `apply` command will:

1. Render manifests (like `nyl render`)
2. Connect to the Kubernetes cluster
3. Apply changes using kubectl apply semantics
4. Report on changes made

This command will be implemented in Phase 4.

## Planned Features

- Kubernetes cluster connection
- Safe apply with server-side apply
- Dry-run mode
- Progress reporting
- Rollback support

## Coming Soon

This command is planned for Phase 4 of the Rust rewrite.
