# Commands

nyl provides several commands for managing Kubernetes manifests:

## Available Commands

### Phase 1 (Current)

- [`new`](./new.md) - Create new projects and components
- [`validate`](./validate.md) - Validate project configuration

### Phase 2+ (Coming Soon)

- [`render`](./render.md) - Render Kubernetes manifests
- [`diff`](./diff.md) - Show diff between rendered manifests and cluster state
- [`apply`](./apply.md) - Apply rendered manifests to the cluster

## Global Options

### `--verbose` / `-v`

Enable verbose logging for debugging.

```bash
nyl --verbose validate
nyl -v new project my-app
```

### `--help` / `-h`

Show help information for any command.

```bash
nyl --help
nyl new --help
nyl validate --help
```

### `--version` / `-V`

Show the version of nyl.

```bash
nyl --version
```
