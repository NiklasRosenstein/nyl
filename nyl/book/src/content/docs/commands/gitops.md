---
title: 'Rendered GitOps commands'
---

## `nyl target list`

List every discovered target with its Cluster, publication repository, revision,
and path prefix.

```bash
nyl target list
```

## `nyl render-tree`

Compile a target into a destination worktree. The target path prefix is appended
to `--output-dir`.

```bash
nyl render-tree --target production --output-dir deploy
nyl render-tree --target production --output-dir deploy --check
nyl render-tree --target production --output-dir deploy --force
nyl render-tree --target production --output-dir deploy --refresh
nyl render-tree --target production --output-dir deploy --no-cache
```

`--check` renders and validates without writing files.
By default, reconciliation rejects indexed files that are missing or differ
from their recorded digest. `--force` warns and recreates those owned files from
the current render. It does not overwrite unowned paths, cross ownership
boundaries, or permit symbolic-link traversal.

`--refresh` bypasses render cache reads and repopulates successful entries.
`--no-cache` uses ephemeral source storage and performs no persistent cache
reads or writes. These mutually exclusive flags are also available on
`diff-tree` and `publish-tree`.

All three tree commands report Release progress on stderr. The default
`--progress auto` displays an updating bar when stderr is attached to a terminal
and prints one line as each Release starts in CI or other non-interactive
environments. Use `--progress bar` or `--progress plain` to select a presentation
explicitly, or `--progress off` to disable it. A complete target-tree cache hit
has no per-Release work to report.

## `nyl diff-tree`

Compare the current desired tree with the published destination revision:

```bash
nyl diff-tree --target production --against published
nyl diff-tree --target production --against published --refresh
```

An existing published target prefix must contain a valid ownership index, and
every indexed file must match its recorded digest. Unindexed repository content
is never treated as target-owned baseline data.

Compare with a render from a source revision:

```bash
nyl diff-tree \
  --target production \
  --against source \
  --source-ref origin/main
```

The command writes a multi-file unified diff to stdout. Source-derived diffs
also compare the cluster, repository, revision, and path prefix through a synthetic
`_nyl/publication.json` diff entry. `--fail-on-diff` gives CI a non-zero result
while retaining the diff output. Mutable comparison refs must refresh
successfully; cached refs are not accepted as current state.

## `nyl source update`

Resolve remote ApplicationGroup revisions and update their full commit locks:

```bash
nyl source update
nyl source update workloads
nyl source update --check
```

Mutable revisions must refresh successfully before a lock is reported current
or updated. Immutable commit rendering can use an existing local cache.

## `nyl publish-tree`

Render and publish one destination branch with compare-and-swap protection:

```bash
nyl publish-tree --target production
nyl publish-tree --target production --dry-run
nyl publish-tree --target production --no-cache
```

The source worktree must be clean and committed. Nyl clones the destination
branch, reconciles indexed files, commits the result, fetches the branch again,
and refuses to push when its remote tip changed.

See [Rendering, diffing, and publishing](/nyl/deployment-workflows/rendered-manifests/rendering-and-publishing/)
for CI patterns and rendered layout. The
[Rendered GitOps resource reference](/nyl/reference/resources/gitops/)
documents the configuration model.
