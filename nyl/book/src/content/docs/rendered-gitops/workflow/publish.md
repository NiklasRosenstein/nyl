---
title: 'Publish from the Protected Branch'
sidebar:
  label: 'Publish'
---

```bash
nyl publish-tree --target production
```

The source worktree must have a committed revision. A dirty worktree triggers a
second render from a temporary clean checkout of `HEAD`. Nyl publishes the
clean result when its rendered files and publication coordinates match the
working-tree result. Use `--require-clean` to reject every non-ignored change,
or `--allow-dirty` to publish a differing working-tree render with explicit
dirty provenance. These options are mutually exclusive.

Nyl clones the destination revision, reconciles only indexed files, creates one
commit, refreshes the remote revision, and performs a compare-and-swap push. It
refuses to push when the remote tip changes concurrently. The result identifies
the destination repository, branch, and commit. The commit message records the
source repository and commit plus the deployment target and cluster as Git
trailers. Dirty publications also include `Nyl-Source-Dirty: true`, and their
ownership index records `dirty: true`.

An interrupted local reconciliation can resume when installed files match the
intended generation. Unrelated modifications and symbolic-link ancestors fail
closed. Git credentials and protected-branch rules remain forge configuration.


See [`publish-tree`](/nyl/reference/cli/publish-tree/) for all flags and the
[security guide](/nyl/rendered-gitops/security/) for ownership and publication
boundaries.
