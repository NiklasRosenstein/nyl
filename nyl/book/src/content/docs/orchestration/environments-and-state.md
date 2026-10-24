---
title: 'Environments and state'
---

An [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/) is one
place orchestration reconciles units into. Its state lives in Git: a desired
ref and an observed ref, by default the branches `nyl/<environment>/desired`
and `nyl/<environment>/observed`. Each operation writes at most one commit per
state ref, so the history of those branches is the reviewable record of what
Nyl decided and observed. [`nyl state`](/nyl/reference/cli/state/) creates,
moves, and forgets that state.

## Where state lives

The repository is the Environment's `state.repositoryRef` or
`state.repository`, else that of the EnvironmentGroup with shared fields that
selects it, else the checkout's `origin`, else the clone itself. Without a
remote, the clone's own branches are the state. A GitRepository's
`publishURL` is used for pushes.

Runs take the environment's lease, `refs/nyl/<environment>/lease` under the
default `coordinationRefPrefix`, so only one run changes an environment at a
time. Every state change reaches the state refs in one atomic push together
with a compare-and-swap of that lease, so a run whose lease was taken over can
never overwrite state. Nyl never force-pushes a state ref. Protect the state
branches against force pushes and deletion, and give push access to the CI
identity and the operators who run `nyl` commands.

## Git host compatibility

The state repository's host must accept two things:

- refs under the coordination prefix, by default the custom namespace
  `refs/nyl/`, created and deleted on every run and listed by pattern;
- atomic pushes that move a state branch and a lease ref together, each under
  its own lease, where one stale lease rejects the whole push.

These hosts were verified with both the default prefix and the branch prefix
`refs/heads/nyl-run/`:

| Host | Version | `refs/nyl/` | `refs/heads/nyl-run/` |
|---|---|---|---|
| GitHub (github.com) | hosted, October 2026 | ✓ | ✓ |
| GitLab (gitlab.com) | hosted, October 2026 | ✓ | ✓ |
| Forgejo | 11.0.6 | ✓ | ✓ |
| Plain Git (`file://`) | 2.43 | ✓ | ✓ |

On a host that rejects refs outside `refs/heads/`, set
`state.coordinationRefPrefix: refs/heads/nyl-run/` on the Environment. A host
without atomic pushes cannot hold orchestration state.

<details>
<summary>Checking another host</summary>

Run these pushes against a scratch repository on the host. `B` is a branch
such as `refs/heads/nyl-check/desired`, `L` a lease ref such as
`refs/nyl/nyl-check/lease`, and `s1`, `l1`, `s2`, `l2` are commits where `s2`
and `l2` descend from `s1` and `l1`.

1. Create both refs: `git push --atomic --force-with-lease=B: --force-with-lease=L: <url> s1:B l1:L`.
2. A stale lease must reject the whole push, leaving both refs unchanged:
   `git push --atomic --force-with-lease=B:s1 --force-with-lease=L:s1 <url> s2:B l2:L`.
3. Matching leases move both refs: `git push --atomic --force-with-lease=B:s1 --force-with-lease=L:l1 <url> s2:B l2:L`.
4. The lease ref is listed by pattern: `git ls-remote <url> 'refs/nyl/nyl-check/*'`.
5. Both refs are deleted together: `git push --atomic --force-with-lease=L:l2 --force-with-lease=B:s2 <url> :L :B`.

Repeat with `L` under `refs/heads/nyl-run/` to check the branch prefix.

</details>
