---
title: 'publish-tree'
---

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
rendering. Its error lists up to 20 dirty files with Git index/worktree status
codes and repository-relative paths, and reports how many more were omitted.
It identifies the repository root so the full status can be inspected even when
Nyl is invoked from a project subdirectory.

`--allow-dirty` skips the clean-checkout comparison and publishes the
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

See [Publish from the protected branch](/nyl/rendered-gitops/workflow/publish/)
for CI patterns and rendered layout. The
[Rendered GitOps resource reference](/nyl/rendered-gitops/resources/)
documents the configuration model.

See the [options shared by the tree commands](/nyl/reference/cli/render-tree/#options-shared-by-the-tree-commands) for target selection, caching, secret inputs, progress,
and validation.

## Partial publication

Publish selected ApplicationGroups or individual Releases:

```bash
nyl publish-tree --target production --application-group payments
nyl publish-tree --target production --release payments/api
nyl publish-tree --target production --release payments/api --release payments/worker --dry-run
```

Both selectors are repeatable and combine by union. Release names must be
`<applicationGroup>/<release>` and require a literal Release `metadata.name`,
so selection does not evaluate other Release templates. Without selectors Nyl
publishes the full target.

Nyl renders the selection and reads the remaining manifests from a verified,
pinned destination commit. Whole-tree ownership, Namespace, CRD, project, and
configured manifest validation checks use the combined candidate. Unselected
Release templates and inputs are not evaluated; static project discovery and
control-resource validity still apply. The summary identifies the baseline,
rendered/retained/removed Release counts, and changes to shared catalog artifacts.

Selecting a group replaces its published membership and can remove Releases.
Selecting one Release requires it to exist and preserves every other Release.
Retained workloads and Applications keep their bytes and original provenance.
When an update requires changing a retained owner or shared carried state,
select the affected consumers too. Shared generated projects may change to admit
the combined candidate's namespaces and resource kinds.

Run one full publication before using partial publication on an older tree.
Partial publication requires version 3 ownership metadata and refuses missing or
externally modified indexed files. Target-wide configuration changes require a
full publication; group configuration changes require selecting the whole group.
A destination update during rendering fails publication; rerun to render against
the new baseline. `--dry-run` prepares the complete candidate without pushing.

Partial trees can contain several source revisions. Their index records source
and input digests per Release, without copying input values or defaults. The
publication commit's source trailer describes this
operation, not every retained Release. A partial publication does not establish
that the whole target matches that source revision.
