---
title: 'Rendered GitOps commands'
---

## `nyl get targets`

List every discovered target with its Cluster, publication repository, revision,
and path prefix.

```bash
nyl get targets
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

Tree rendering excludes the project secrets provider and `NYL_*` process
environment by default. Pass `--allow-secret-inputs` to the individual
`render-tree`, `diff-tree`, or `publish-tree` invocation when trusted central
project templates intentionally depend on them. Explicit Kubernetes `Secret`
manifests remain ordinary input resources. Remote and independently controlled
source sessions never receive project secrets or process environment, including
when this flag is set.

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
stderr. By default, stdout contains only multi-file unified diff bytes; a
comparison with no differences produces no stdout. Use `--output` to write the same bytes
atomically to a file instead:

```bash
nyl diff-tree --target production --output rendered.diff
nyl diff-tree --target production --output rendered.diff --fail-on-diff
```

A successful comparison with no differences creates an empty output file.
Rendering or comparison errors leave existing output files untouched.
`--fail-on-diff` writes the complete diff and all requested reports before
returning a non-zero status when changes exist.

### Statistics and report exports

The complete report includes comparison context, file and line counts, and
render/cache/source statistics. Totals distinguish added, modified, and deleted
files. `--stats-files` includes a path-sorted table with each file's status and
added/removed line counts in human-readable reports:

```bash
nyl diff-tree --target production --stats-files
```

Line counts use the same line comparisons as the patch, excluding patch headers
and context. Replacements count as deletions plus insertions; provenance comments
count as ordinary lines. Counts respect the selected catalog or Application
view. Empty-file additions and deletions count as file changes with zero changed
lines. Invalid UTF-8 or NUL-containing files are binary: their changes produce
patch notices, and their line counts are unavailable. Aggregate line counts
cover text changes only. File moves count as additions and deletions.

Use repeatable `--stats-output FORMAT:PATH` options to export `text`, `markdown`,
and/or `json` reports independently of the patch. Exports copy the report; add
`--no-stats-stderr` to suppress the stderr copy. Progress and errors remain on
stderr; `--progress off` disables render progress. Formatter preferences are
command-line options only.

For example, prepare a Markdown PR comment and JSON for subsequent processing:

```bash
nyl diff-tree --target production \
  --output artifacts/rendered.diff \
  --stats-files \
  --stats-output markdown:artifacts/comment.md \
  --stats-output json:artifacts/stats.json \
  --no-stats-stderr
```

Nyl writes these artifacts locally; a CI step can post `comment.md` as a sticky
PR comment. Add `--fail-on-diff` if the job should fail when changes exist; the
artifacts are still written before that failure.

Paths resolve against the invocation directory. `PATH=-` selects stdout, so
redirect the diff to a file when exporting a report to stdout:

```bash
nyl diff-tree --output rendered.diff --stats-output json:- --no-stats-stderr
```

On Unix, `/dev/null` discards an output. For example, display just the Markdown
report on stdout while discarding the patch:

```bash
nyl diff-tree --stats-output markdown:- --output /dev/null --no-stats-stderr
```

Multiple outputs may use `/dev/null`, but cannot share stdout or the same regular
file. Nyl creates missing parent directories and atomically replaces each output
file. A write failure
returns an error; multiple output files are not a single transaction, so files
already written remain available. A successful zero-change comparison writes an
empty patch and a complete report with zero counts and an empty file list.

Text reports on stderr follow `--color auto|always|never`: automatic color on a
TTY or in CI, respecting `NO_COLOR`, `CLICOLOR`, `CLICOLOR_FORCE`, and `TERM`.
Added lines are green and removed lines red. Text file exports are plain in
auto mode; `--color always` retains ANSI styling. Markdown and JSON never contain
formatter ANSI sequences.

### JSON report contract

JSON always includes every changed file, independently of `--stats-files`. The
report has `schema_version: 1` and these top-level objects:

- `comparison`: `target`, `selection`, `desired_source`, `baseline`,
  `desired_publication`, and `diff_output` (`-` means stdout).
- `diff`: `has_changes`, `files_changed`, `files_added`, `files_modified`,
  `files_deleted`, `binary_files`, `lines_added`, `lines_removed`, and `files`.
  Each file has `path`, `status` (`added`, `modified`, or `deleted`), `binary`,
  `lines_added`, and `lines_removed`. Binary line counts are `null`.
- `render`: `layers`, `target_reuse`, `release_helm_renders_avoided`, and `sources`.
  Statistics aggregate the complete invocation, including both renders for a
  source comparison.

`selection.mode` is `tree`, `catalog`, or `applications`; the last includes an
`applications` array of explicit selectors, empty for all workload Applications.
`desired_source` contains nullable `repository` and `commit`, plus `dirty`.
`baseline.mode` is `published` or `source`; both include the resolved `commit`
and a `publication`. A source baseline also includes `repository` and `revision`.
Publication objects contain `cluster`, sanitized `repository`, nullable
`publish_url`, `revision`, and `path_prefix` (empty means repository root).
Reports contain comparison metadata and counts, not manifest contents.

Render `layers` keys are `target`, `release`, and `helm`; each entry has `outcomes`
and `bypass_reasons` count maps. Outcomes are `hit`, `miss`, `invalidated`,
`bypassed`, `refreshed`, `stored`, and `corrupt`. `target_reuse` is nullable; when
present it contains `releases` and `helm_renders` avoided by target reuse.
`release_helm_renders_avoided` counts Helm renders avoided by release reuse.
`sources` is a count map with snake_case operation keys: `remote_manifest_download`,
`remote_manifest_reuse`, `helm_chart_pull`, `helm_chart_reuse`, `git_source_reuse`,
`vendor_artifact_reuse`, `git_repository_clone`, `git_repository_reuse`,
`git_ref_refresh`, `git_worktree_create`, and `git_worktree_reuse`. Absent count-map
entries mean zero. Bypass-reason strings are descriptive, not stable identifiers.
Consumers should ignore additional fields; incompatible changes require a new
schema version.

### Selecting files

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
views leave those coordinates in the comparison report. Mutable comparison refs
must refresh successfully; cached refs are not accepted as current state.

## `nyl update source-locks`

Resolve remote ApplicationGroup revisions and update their full commit locks:

```bash
nyl update source-locks
nyl update source-locks workloads
nyl update source-locks --check
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

## Manifest validation

Use `--validate` to run project-configured validators. See
[Manifest validation](../manifest-validation/) for automatic validation, captured
CRD schemas, inherited Cluster contracts, and offline schema vendoring.
