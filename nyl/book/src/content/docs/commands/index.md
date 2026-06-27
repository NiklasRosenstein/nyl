---
title: 'Commands'
---

nyl provides several commands for managing Kubernetes manifests:

## Available Commands

### Phase 1 (Current)

- [`new`](/nyl/commands/new/) - Create new projects and components
- [`validate`](/nyl/commands/validate/) - Validate project configuration

### Phase 2+ (Coming Soon)

- [`rendering-pipeline`](/nyl/commands/rendering-pipeline/) - Shared rendering pipeline used by render/diff/apply
- [`render`](/nyl/commands/render/) - Render Kubernetes manifests
- [`diff`](/nyl/commands/diff/) - Show diff between rendered manifests and cluster state
- [`apply`](/nyl/commands/apply/) - Apply rendered manifests to the cluster
- [`release`](/nyl/commands/release/) - Inspect release history and roll back to a previous revision
- [`cluster-info`](/nyl/commands/cluster-info/) - Print Kubernetes version and API versions for offline rendering

## Global Options

### `--verbose` / `-v`

Enable verbose logging for debugging.

```bash
nyl --verbose validate
nyl -v new project my-app
```

### `--color <COLOR>`

Control when to use colored output. Accepts three values:

- `auto` (default) - Automatically detect if colors should be used based on TTY detection
- `always` - Always use colors, even when output is redirected to a file or pipe
- `never` - Never use colors

This applies to both:
- Colored output from commands (diffs, status indicators, etc.)
- Log messages from `tracing` (INFO, WARN, ERROR)

This flag is particularly useful when:
- Redirecting output to a file where ANSI color codes are not desired
- Working in environments where terminal color support is inconsistent
- Forcing colored output in CI/CD pipelines that support ANSI colors

```bash
# Disable colors when piping to a file
nyl --color never diff | tee output.txt

# Force colors in CI/CD
nyl --color always diff

# Auto-detect (default behavior)
nyl diff
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
