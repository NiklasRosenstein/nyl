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
