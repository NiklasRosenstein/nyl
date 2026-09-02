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

`--target` may be omitted when exactly one DeploymentTarget is configured. With
multiple targets, Nyl requires an explicit selection and lists the available
names. The same rule applies to `diff-tree` and `publish-tree`.

`--check` renders and validates without writing files.
By default, reconciliation rejects indexed files that are missing or differ
from their recorded digest. `--force` warns and recreates those owned files from
the current render. It does not overwrite unowned paths, cross ownership
boundaries, or permit symbolic-link traversal.
An existing ownership index is checked against the selected target before Nyl
renders any Releases or Helm charts.

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

The command identifies the desired source commit and exact baseline repository,
revision, resolved commit, path, selected view, and output destination on
stderr. Stdout contains only multi-file unified diff bytes; a comparison with
no differences produces no stdout. Use `--output` to write the same bytes
atomically to a file instead:

```bash
nyl diff-tree --target production --output rendered.diff
nyl diff-tree --target production --output rendered.diff --fail-on-diff
```

A successful comparison with no differences creates an empty output file.
Errors leave an existing output file untouched, and `--fail-on-diff` writes the
complete diff before returning a non-zero status.

Limit the comparison to the generated catalog, all workload Applications, or
specific Argo CD Application identities:

```bash
nyl diff-tree --target production --catalog
nyl diff-tree --target production --applications
nyl diff-tree --target production \
  --application argocd/rise \
  --application argocd/loki
```

An Application view contains the generated Application manifest and its plain
directory payload. Nyl derives these views from the generated Applications, so
custom `ApplicationGroup.spec.outputPath` values remain supported without
additional ownership-index metadata. The parent catalog Application is omitted
from `--applications` but can be selected explicitly. `--catalog` conflicts
with the Application filters.

Source-derived whole-tree diffs also compare the cluster, repository, revision,
and path prefix through a synthetic `_nyl/publication.json` diff entry. Scoped
views leave those coordinates in the stderr summary. Mutable comparison refs
must refresh successfully; cached refs are not accepted as current state.

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
nyl publish-tree --target production --require-clean
nyl publish-tree --target production --allow-dirty
```

The source worktree must have a committed revision. When it is dirty, Nyl
renders the selected target again from a temporary clean checkout of `HEAD`.
Publication proceeds when both rendered trees and their publication coordinates
match, using the clean checkout for provenance. Local files such as editor
configuration therefore do not block a reproducible publication.

`--require-clean` rejects any non-ignored source worktree change before
rendering. `--allow-dirty` skips the clean-checkout comparison and publishes the
working-tree render explicitly; the ownership index records `dirty: true` and
the commit message includes `Nyl-Source-Dirty: true`. The two options are
mutually exclusive.

Nyl clones the destination branch into a clean checkout, reconciles indexed
files, stages only the selected target prefix, commits the result, fetches the
branch again, and refuses to push when its remote tip changed. A publication
branch that does not exist starts as an empty branch rather than inheriting the
repository's default branch.
The completion summary names the destination repository, branch, and resulting
commit. Publication commit messages carry `Nyl-Source-Repository`,
`Nyl-Source-Commit`, `Nyl-Deployment-Target`, and `Nyl-Cluster` provenance
trailers. `--message` replaces the subject while retaining those trailers.

Publication commits use the normal Git author identity from `GIT_AUTHOR_NAME` /
`GIT_AUTHOR_EMAIL` or `user.name` / `user.email`. Set
`NYL_GIT_AUTHOR_NAME` and `NYL_GIT_AUTHOR_EMAIL` for a Nyl-specific override.

See [Rendering, diffing, and publishing](/nyl/deployment-workflows/rendered-manifests/rendering-and-publishing/)
for CI patterns and rendered layout. The
[Rendered GitOps resource reference](/nyl/reference/resources/gitops/)
documents the configuration model.
