---
title: 'render-tree'
---

Compile a target into a destination worktree. The target path prefix is appended
to `--output-dir`.

```bash
nyl render-tree --target production --output-dir deploy
nyl render-tree --target production --output-dir deploy --check
nyl render-tree --target production --output-dir deploy --force
nyl render-tree --target production --output-dir deploy --refresh
nyl render-tree --target production --output-dir deploy --no-cache
nyl render-tree --target production --output-dir deploy --offline
```

When a target has `fromPublication` Release inputs, `render-tree` fetches its
publication branch and reports the commit it read the state from; `--offline`
reads the cached branch head instead and says so.

`--check` renders and validates without writing files.
By default, reconciliation rejects indexed files that are missing or differ
from their recorded digest. `--force` warns and recreates those owned files from
the current render. It does not overwrite unowned paths, cross ownership
boundaries, or permit symbolic-link traversal.
An existing ownership index is checked against the selected target before Nyl
renders any Releases or Helm charts.

## Options shared by the tree commands

`render-tree`, [`diff-tree`](/nyl/reference/cli/diff-tree/), and
[`publish-tree`](/nyl/reference/cli/publish-tree/) share these options.

`--target` may be omitted when exactly one DeploymentTarget is configured. With
multiple targets, Nyl requires an explicit selection and lists the available
names.

`--refresh` bypasses render cache reads and repopulates successful entries.
`--no-cache` uses ephemeral source storage and performs no persistent cache
reads or writes. The two flags are mutually exclusive.

Tree rendering excludes the project secrets provider and `NYL_*` process
environment by default. Pass `--allow-secret-inputs` to the individual
`render-tree`, `diff-tree`, or `publish-tree` invocation when trusted central
project templates intentionally depend on them. Explicit Kubernetes `Secret`
manifests remain ordinary input resources. Remote and independently controlled
source sessions never receive project secrets or process environment, including
when this flag is set.

Each tree command reports Release progress on stderr. The default
`--progress auto` displays an updating bar when stderr is attached to a terminal
and prints one line as each Release starts in CI or other non-interactive
environments. Use `--progress bar` or `--progress plain` to select a presentation
explicitly, or `--progress off` to disable it. A complete target-tree cache hit
has no per-Release work to report.

Use `--validate` to run project-configured validators. See
[Manifest validation](/nyl/kubernetes/validation/manifest-validation/) for automatic validation, captured
CRD schemas, inherited Cluster contracts, and offline schema vendoring.
