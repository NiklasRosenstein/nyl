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
Orchestration is being built in milestone M3. These commands create,
maintain, and repair state; [`nyl reconcile`](/nyl/reference/cli/reconcile/) runs units
against it.
:::

See [Environments and state](/nyl/orchestration/environments-and-state/) for
where state lives, how the lease protects it, and which Git hosts support it.

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

## `nyl state reset`

```bash
nyl state reset -e dev --local
nyl state reset -e dev --local --discard network,database
nyl state reset -e dev --local --discard-all
```

Drops this clone's [local transitions](/nyl/orchestration/environments-and-state/#local-runs),
the commits `nyl reconcile --local` wrote to the clone's `nyl/<env>/*`
branches, and moves those branches back to the remote state refs. It writes
only the clone's branches: it takes no lease and never changes remote state,
so `--local` is required.

Forgetting a local receipt can leave resources behind, because the remote
state does not record them. When a unit's receipt or condition in the local
view differs from the remote's, the command lists those units, exits 1 with
`NYL-STATE-RESET-UNDISCARDED`, and proceeds only once `--discard <unit>…`
(repeatable or comma-separated) or `--discard-all` covers them. Let the remote
record the change first, with `nyl reconcile -e <env> --unit <unit>` after the
change is committed, or undo the local effects.

The report lists the units discarded and the local commits dropped. When the
branches already follow the remote, nothing changes. State that lives on the
clone's own branches, without a remote, exits 1 with
`NYL-STATE-RESET-UNSUPPORTED`.

## `nyl state restore`

```bash
nyl state restore -e dev --to 41f0c2e9b7d14c3a8e2f6b1d0c9a7e5f3b2d1c0a --reason "local transitions were pushed"
```

Restores the content of an earlier state commit after local branches were
pushed to the remote state refs by hand. Such a push puts `Nyl-Local`
commits at the remote tips, and every state-writing command then exits 1
with `NYL-STATE-LOCAL-TIP`, naming the last commit without the trailer;
pushed local transitions are never accepted, because their source commits may
be unpushed or dirty.

`--to` takes a full commit ID of the desired or the observed state ref. The
command pairs it with the other ref's commit of the same transition, the one
with the same `Nyl-Run-Id` trailer, else the one its `Nyl-Read-Desired` or
`Nyl-Read-Observed` trailer names, so it never restores a desired and
observed pair that did not exist together. A commit outside the state refs, a
`Nyl-Local` commit, or one whose pair cannot be found exits 1 with
`NYL-STATE-RESTORE-INVALID`.

It takes the lease and writes one desired and one observed commit on top of
the current tips, so protected branches need no force push. The commits
restore every unit document, artifact, observation, and attestation of that
commit, carry `state.yaml` forward with a `state-restored` event, and list
the rejected local commits. Admission is never restored. When the state
already holds that content and its tips are not local, nothing changes.

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
to wait for another run's lease, `--author "Name <email>"`, and `-o json` for
the operation's versioned report.

`--author` names the author and committer of the state commits the command
writes. Without it, Nyl uses Git's `user.name` and `user.email` for the
project's repository; without either, the command exits 1 with
`NYL-COMMIT-IDENTITY-MISSING` before changing anything. The commit time comes
from the same clock as the command's other time-based decisions. `state
reset` writes no commit, but still resolves the identity.

They exit 0 when done or when nothing needed to change, 2 while another run
holds the lease (`NYL-LEASE-HELD`), and 1 for every other refusal; each error
and warning carries a stable code and its fix. Every command that writes
remote state, except `state restore`, exits 1 with `NYL-STATE-LOCAL-TIP` when
a state ref's tip is a pushed local transition.
