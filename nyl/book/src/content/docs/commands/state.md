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

GitHub, GitLab, and Forgejo accept the lease refs and atomic pushes Nyl needs.
On a host that rejects refs outside `refs/heads/`, set
`state.coordinationRefPrefix: refs/heads/nyl-run/` on the Environment.

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
