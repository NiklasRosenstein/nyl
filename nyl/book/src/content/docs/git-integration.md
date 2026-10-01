---
title: 'Git Integration'
---

Nyl resolves Git-backed Helm charts and rendered GitOps publication sources
through a shared local cache.

## Cache directory

Set `NYL_CACHE_DIR` to choose the cache root. Nyl otherwise uses
`.nyl/cache/` beneath the project root.

```bash
export NYL_CACHE_DIR="$HOME/.cache/nyl"
```

Bare repositories share Git objects across isolated worktrees for different
revisions. Cached checkouts are renderer-owned and must not contain local
changes.

## Git-backed Helm charts

Prefix a repository with `git+` and select the chart directory with `name`:

```yaml
apiVersion: k8s.nyl/v1
kind: HelmChart
metadata:
  name: application
  namespace: default
spec:
  chart:
    repository: git+https://github.com/example/charts.git
    version: v2.1.0
    name: charts/application
```

`version` accepts a branch, tag, or commit SHA and defaults to `HEAD`. Pin a
commit for reproducible output. Helm dependencies in the selected chart are
resolved as part of rendering.

## Authentication

Nyl runs the `git` command line for every fetch, ref listing, and push, so it
authenticates exactly as `git` does in the same environment: OpenSSH reads
`~/.ssh/config` (`Host` aliases, `IdentityFile`, `IdentitiesOnly`) and the SSH
agent, and HTTPS uses your credential helpers. `url.<base>.insteadOf`,
`core.sshCommand`, and `GIT_SSH_COMMAND` apply as well. Publication uses the
same mechanism. Nyl never waits for an HTTPS password prompt.

CI jobs that access private repositories should provide an SSH agent, an SSH
key with matching SSH config, or a credential helper, and keep credentials out
of Nyl manifests.

## Cache behavior

Nyl refreshes remote refs before resolving mutable branch and tag names. A
normal render may fall back to a cached ref when the remote is unavailable.
Freshness-sensitive operations, including source comparisons and lock updates,
require a successful refresh.

Immutable commit rendering uses objects already present in the cache without
refreshing refs. `diff-tree --offline` never fetches: it compares against the
cached publication and source refs.
`--refresh` bypasses exact source and render cache reads, while `--no-cache`
uses disposable storage and performs no persistent cache reads or writes.

## Troubleshooting

For cache permission errors, set `NYL_CACHE_DIR` to a writable directory. For
authentication failures, run `git ls-remote <url>` in the same environment;
Nyl succeeds wherever that does.

Delete only the affected Nyl cache entry when diagnosing corrupt local Git
state; Nyl will clone the repository again on the next access.
