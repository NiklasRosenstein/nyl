---
title: 'Commands'
---

Commands grouped by what they are for. Each page documents one command or a
closely related pair.

## Available Commands

- **Project setup**: create and check a project.
  - [`init`](/nyl/reference/cli/init/) - Initialize a Nyl project
  - [`create and delete`](/nyl/reference/cli/create/) - Add or remove GitOps resource source files
  - [`validate`](/nyl/reference/cli/validate/) - Validate project configuration
  - [`schema`](/nyl/reference/cli/schema/) - Generate project and resource schemas for editors and CI
- **Project inputs**: inspect resources and pin what rendering reads.
  - [`get and update`](/nyl/reference/cli/project-resources/) - Inspect resources, capture cluster capabilities, and move source locks
  - [`vendor`](/nyl/reference/cli/vendor/) - Snapshot and verify remote renderer inputs
- **Kubernetes**: act on a cluster directly, through the shared [rendering pipeline](/nyl/kubernetes/pipeline/).
  - [`render`](/nyl/reference/cli/render/) - Render Kubernetes manifests
  - [`diff`](/nyl/reference/cli/diff/) - Show diff between rendered manifests and cluster state
  - [`apply`](/nyl/reference/cli/apply/) - Apply rendered manifests to the cluster
  - [`release`](/nyl/reference/cli/release/) - Inspect release history and roll back to a previous revision
- **Rendered GitOps**: render, compare, and publish a deployment target's tree.
  - [`render-tree`](/nyl/reference/cli/render-tree/) - Render a deployment target into a destination tree
  - [`diff-tree`](/nyl/reference/cli/diff-tree/) - Compare a target's desired tree with its publication or a source revision
  - [`publish-tree`](/nyl/reference/cli/publish-tree/) - Publish a target's tree to its branch with compare-and-swap
- **CI integration**: report results where reviewers look.
  - [`comment`](/nyl/reference/cli/comment/) - Maintain sticky PR/MR comments on GitHub, GitLab, and Forgejo
- **Orchestration** (in development): manage environments' state in Git.
  - [`plan`](/nyl/reference/cli/plan/) - Show what a reconcile would do, without writing anything
  - [`reconcile`](/nyl/reference/cli/reconcile/) - Execute an environment's ready units and record the results
  - [`status`](/nyl/reference/cli/status/) - Report each unit's state from one snapshot of an environment's state
  - [`verify`](/nyl/reference/cli/verify/) - Check current receipts for drift and record the observations
  - [`recover`](/nyl/reference/cli/recover/) - Clear a unit's uncertain or failed condition, or accept a checkpointed result
  - [`state and lease`](/nyl/reference/cli/state/) - Create, move, forget, reset, and restore an environment's state; break a lost lease

## Global Options

### `--verbose` / `-v`

Enable verbose logging for debugging.

```bash
nyl --verbose validate
nyl -v init my-app --minimal
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
nyl create --help
nyl validate --help
```

### `--version` / `-V`

Show the version of nyl.

```bash
nyl --version
```
