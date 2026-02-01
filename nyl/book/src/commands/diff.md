# diff

> **Status**: Phase 4 (Not yet implemented)

Show the difference between rendered manifests and the current cluster state.

## Synopsis

```bash
nyl diff [options]
```

## Description

The `diff` command will:

1. Render manifests (like `nyl render`)
2. Connect to the Kubernetes cluster
3. Fetch current resource states
4. Show differences using a diff format

This command will be implemented in Phase 4.

## Planned Features

- Kubernetes cluster connection
- Resource state fetching
- Smart diff formatting
- Dry-run validation
- Change summary

## Coming Soon

This command is planned for Phase 4 of the Rust rewrite.
