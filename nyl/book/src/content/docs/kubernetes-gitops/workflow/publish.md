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


## Git LFS in CI

When the source repository tracks files with Git LFS, such as vendored Helm
chart archives, the container that runs Nyl needs Git LFS too. Git compares an
LFS file with its committed pointer through the LFS clean filter. Without it,
Git compares the hydrated file itself, so an unchanged archive can appear
modified and `--require-clean` rejects the worktree. This can be intermittent:
Git trusts its cached file metadata until something, such as a timestamp-only
change, makes it compare content.

CI systems that check the repository out in a separate container hide the
missing tool. GitLab Runner's
[helper container](https://docs.gitlab.com/runner/configuration/advanced-configuration/#helper-image)
can hydrate LFS files successfully while the job's own image has no Git LFS, and
rendering still works because Nyl reads the hydrated files. In the job's image:

- install `git-lfs` so it is on the `PATH` Nyl runs Git with, and
- run `git lfs install --local --skip-repo` in the checkout to configure its
  filters.

The [Nyl CI image](https://github.com/NiklasRosenstein/nyl/tree/develop/docker)
includes Git LFS and configures its filters system-wide. Keep `--require-clean`
enabled: the fix is to make Git see the checkout correctly, and Nyl never
exempts LFS files from the cleanliness check. When changed files are tracked by
Git LFS and Git cannot run its filter, the `--require-clean` error says which
of the two is missing.

See [`publish-tree`](/nyl/reference/cli/publish-tree/) for all flags and the
[security guide](/nyl/kubernetes-gitops/security/) for ownership and publication
boundaries.

## Application retirement

Automatic catalog synchronization can run with manual pruning. AppProject updates
still take effect independently of pruning, so retain cleanup scope before
retiring Applications. `diff-tree` fails unsafe scope contractions in MR CI, and
publication checks the same rule before committing or pushing. Follow
[Retiring Applications safely](/nyl/kubernetes-gitops/resource-guides/application-group/#retiring-applications-safely),
then verify live cascading deletion before releasing retained permissions.
