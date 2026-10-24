---
title: 'state and lease'
---

An [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/) keeps
its orchestration state in Git: a desired ref and an observed ref, by default
the branches `nyl/<environment>/desired` and `nyl/<environment>/observed` of
the repository its `state` names. Each operation writes at most one commit per
state ref, so the history of those branches is the reviewable record of what
Nyl decided and observed.

:::note
Orchestration is being built in milestone M3. These commands create and
maintain state; `nyl reconcile` and the other run commands follow.
:::

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

## `nyl state init`

```bash
nyl state init -e dev
nyl state init -e dev --fresh --reason "start over after restoring the cluster"
```

Creates `state.yaml` on the environment's state refs. Running it again
changes nothing. No other command creates state implicitly; a command against
a location without `state.yaml` exits 1 with `NYL-STATE-MISSING` and names
both ways forward. A state repository Nyl cannot reach or read exits 1 with
`NYL-STATE-UNAVAILABLE`. When a commit outside Nyl changes the environment's
state while a command runs, the command exits 1 with `NYL-STATE-CONFLICT`; run
it again. Commits of environments sharing the ref through `path` never cause
this: Nyl rebases past them.

`--fresh` starts over deliberately: every unit becomes a new incarnation and
executes again at the next reconcile.

## `nyl state move`

```bash
# After pointing dev's state at another repository:
nyl state move -e dev --from https://git.example.com/old-state.git
# After renaming dev's state branches in its Environment:
nyl state move -e dev --desired-ref nyl/dev/desired --observed-ref nyl/dev/observed
# After adding a remote to a project whose state was local:
nyl state move -e dev --from local
# After changing coordinationRefPrefix:
nyl state move -e dev --coordination-refs refs/nyl/
```

Moves state to the location the Environment configures. The options name the
**old** location and each defaults to the configured value: `--from` (a URL, a
GitRepository name, or `local`), `--desired-ref`, `--observed-ref`,
`--state-path`, and `--coordination-refs`. Units keep their incarnations and
receipts.

The history of the state refs moves to the new location, the old location
records the move so that every command against it refuses with
`NYL-STATE-MOVED` and names the new one, and only then does the new location
become usable. An interrupted move completes when run again. The old refs are
kept for audit; removing them is a manual step.

## `nyl state forget`

```bash
nyl state forget -e dev --unit cache --reason "removed by hand"
```

Drops a `retained` tombstone or a `pending-teardown` unit from state without
touching its resources. A unit that returns later starts a new incarnation.

## `nyl lease break`

```bash
nyl lease break -e dev --reason "runner was killed"
```

Declares the lease holder gone, for example after a forced kill. The holder is
the lease, or a live run whose lease ref is already gone. The next run
takes over at once instead of waiting for the lease's deadline, and records
who broke the lease and why in its transition commit. A holder that is in fact
still running can still cause effects, but it can no longer write state.

## Options and exit codes

All commands take `-e/--environment`, `--reason`, `--wait-lease <duration>`
to wait for another run's lease, and `-o json` for the operation's versioned
report. They exit 0 when done or when nothing needed to change, 2 while
another run holds the lease (`NYL-LEASE-HELD`), and 1 for every other
refusal; each error and warning carries a stable code and its fix.
