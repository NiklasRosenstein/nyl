---
title: 'Environments and state'
---

An [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/) is one
place orchestration reconciles units into. Its state lives in Git: a desired
ref and an observed ref, by default the branches `nyl/<environment>/desired`
and `nyl/<environment>/observed`. Each operation writes at most one commit per
state ref, so the history of those branches is the reviewable record of what
Nyl decided and observed. [`nyl state`](/nyl/reference/cli/state/) creates,
moves, forgets, resets, and restores that state. [Git storage](/nyl/reference/git-storage/)
lists every ref involved and when to use which repository.

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

## Local runs

Every command that reads an environment's remote state keeps the clone's own
branches of the state refs, `nyl/<environment>/desired` and
`nyl/<environment>/observed` by default, as the local known version of the
state. It fast-forwards them, creating them when needed, never rewrites one
that diverged, and sets no upstream, so a bare `git push` does not publish
them. `git log nyl/dev/desired` works in any clone that ran Nyl.

**Without a remote**, when nothing names a state repository and the checkout
has no `origin`, those branches are the state. Runs are ordinary runs with the
clone's own lease, and `nyl state init -e <env>` creates the state there.

**Adding a remote.** Once the state repository resolves to a remote, a run
finds no state there and exits 1 with `NYL-STATE-MISSING`, naming the move
from the local branches. Move the state with
[`nyl state move -e <env> --from local`](/nyl/reference/cli/state/#nyl-state-move):
it pushes the branches' history to the remote, records the move there, and
leaves the local branches following the remote.

**`nyl reconcile -e <env> --local`** runs orchestration next to remote state
without publishing it, for experiments from a workstation. It continues from
the local branches and commits there with a `Nyl-Local: true` trailer, never
pushing state:

- It takes the remote lease, because the lease is coordination, not state: a
  local run and a CI run of the same environment never execute at once. This
  needs push access to the lease ref. The lease lists no units, since remote
  runs do not notice local effects. `--local-lease` takes the clone's own
  lease instead, for working without that access, and accepts that CI may run
  at the same time.
- Its effects are real, but remote runs do not notice them: a unit whose
  remote receipt is current is not executed again. It warns with
  `NYL-LOCAL-RUN-UNRECORDED` for every unit it executes and names the command
  that records it remotely, `nyl reconcile -e <env> --unit <unit>` once the
  change is committed.
- Units whose recovery policy is not `converge` run locally only when named
  with `--approve <unit>`; otherwise they wait as `awaiting-approval`.
- It never imports remote run refs, so while remote state has runs to take
  over (finished, past their deadline, or the holder of a broken lease) it
  exits 1 with `NYL-LOCAL-RUN-LEFTOVERS` before taking the lease, leaving the
  lease and any break as they were; an ordinary `nyl reconcile -e <env>`
  takes them over first.
- `nyl status` and `nyl get` with `--local` show the local view.

Once the remote moves, the local branches have diverged and Nyl stops
fast-forwarding them. `nyl status -e <env>` warns with
`NYL-STATE-LOCAL-DIVERGED`, such as "local state of dev has 2 local
transitions and is 3 behind the remote".
[`nyl state reset -e <env> --local`](/nyl/reference/cli/state/#nyl-state-reset)
drops the local transitions and follows the remote again, once the remote has
recorded the change or the local effects were undone. It lists the units whose
local receipts the remote does not reflect and proceeds only with `--discard
<unit>…` or `--discard-all`.

Pushing local branches to the remote by hand puts `Nyl-Local` commits at its
tips. Every run and state-writing command against that state then exits 1
with `NYL-STATE-LOCAL-TIP`, naming the last commit without the trailer. When
several environments share the state refs through `path`, each looks only at
its own commits, those whose `Nyl-Environment` trailer names it, so one
environment's pushed local commits refuse only that environment. Then
[`nyl state restore -e <env> --to <commit>`](/nyl/reference/cli/state/#nyl-state-restore)
writes forward commits restoring that commit's content, so protected branches
need no force push.

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
