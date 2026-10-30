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
maintain, repair, and delete state; [`nyl reconcile`](/nyl/reference/cli/reconcile/) runs units
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

When the repository is an absolute local filesystem path or a local `file://`
URL, `state init` also creates a missing bare Git repository and its parent
directories. The read and write endpoints must name the same local path.
Existing Git repositories are reused; an existing file or directory that is
not a Git repository fails with `NYL-STATE-UNAVAILABLE`. Remote repositories
and separate read/write destinations must already exist. Other commands never
create a repository.

`--fresh` starts over deliberately: every unit becomes a new incarnation and
executes again at the next reconcile. It is also the way to start over at a
location whose state was deleted with [`nyl state delete`](#nyl-state-delete):
a plain `state init` there exits 1 with `NYL-STATE-DELETED`.

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

## `nyl state delete`

```bash
nyl state delete -e pr-123 --reason "pull request closed"
nyl state delete -e staging --teardown --reason "decommission staging"
```

Removes an environment's state, for template instances and declared
environments alike, once no unit has an incarnation left. One commit per
state ref, with the operation `state-delete`, removes every file of the
environment's state, including held desired files without an incarnation and
records of units whose resources were left in place, and leaves `state.yaml`
with a `state-deleted` event as the deleted marker. The state refs and their
history stay: on a ref shared through `path`, only the environment's
directory changes, and its own refs keep the history of everything removed.
Evidence and signal refs of the environment are deleted.

It refuses, exit 1, while:

- a unit still has an incarnation, a teardown in progress, or a recorded
  teardown request (`NYL-STATE-DELETE-INCARNATIONS`, listing the units and
  naming `--teardown`, `nyl teardown --unit`, and `nyl state forget`);
- a run ref no run has imported yet exists, because it may be the only
  record of units its run created (`NYL-STATE-DELETE-INCARNATIONS`, naming
  `nyl reconcile -e <env>` and `nyl recover -e <env>`).

`--teardown` runs [`nyl teardown --all`](/nyl/reference/cli/teardown/) first
and continues to the deletion in the same command, once the state holds no
incarnation, no deleting unit, and no recorded teardown request. Otherwise it
stops and deletes nothing: exit 2 when every remaining unit waits (a teardown
wait, an approval, dependents, readiness, or a held unit that still has an
incarnation), and exit 1 when a teardown failed or is uncertain. The
teardown's findings are kept, and its report is under `details.teardown`.
Running the command again resumes. `--allow-dependents`,
`--allow-incomplete`, and `--source <revision>` apply to the teardown and
require `--teardown`. A lease broken with `nyl lease break` whose run is not
imported refuses with `NYL-LEASE-BROKEN` before the teardown.

Every later command for the environment, including `reconcile`, `status`,
and `hold`, exits 1 with `NYL-STATE-DELETED`, naming both ways forward:
removing the Environment from source to decommission it, or `nyl state init
-e <env> --fresh` to start it over. A pipeline that still runs the
environment fails visibly instead of re-creating anything. Nyl reads the
marker from `state.yaml`, never from commit history.

```console
$ nyl state delete -e dev --reason "decommission"
state-delete dev: done
  desired: 412024ed0e8a7fcc259f628136bfb2d34d25973c
  observed: 54e76c2392b75867ce1ef67444c77e4cb3b715de
  removed: ["migrate"]
$ nyl status -e dev
dev: no units
error NYL-STATE-DELETED: the state of dev was deleted with `nyl state delete`
  fix: remove the Environment dev from source to decommission it, or start it over with `nyl state init -e dev --fresh`
$ nyl state init -e dev --fresh --reason "start over"
state-init dev: done
  desired: fc0c49c407eff5f6b2c4e92fea5f530e287aacfa
  observed: e04a73265608a6e3775bbdb42a975e3d58be4578
  fresh: every unit becomes a new incarnation and executes again at the next reconcile
```

## `nyl state reset`

`nyl state reset -e dev --local` follows the configured remote state. If local
receipts are absent from remote state, name the affected units with `--discard`
or explicitly accept all with `--discard-all`. Unpublished run and checkpoint
records must be recovered locally before resetting.

## `nyl state restore`

`nyl state restore -e dev --to <commit>` writes forward commits restoring
historical state content while preserving current admission. The named commit
must belong to the environment's state history.

## `nyl lease break`

```bash
nyl lease break -e dev --reason "runner was killed"
```

Declares the lease holder gone, for example after a forced kill. The holder is
the lease, or a live run whose lease ref is already gone. Only an importer
takes the broken lease over at once, without waiting for its deadline:
`nyl reconcile`, `nyl teardown`, or `nyl recover -e <env>`, which imports
the broken run's results without executing anything. It records who broke
the lease and why in its transition commit. Until then every other state
writer, such as `hold`, `resume`, `verify`, and the state operations,
refuses with `NYL-LEASE-BROKEN` (exit 2), naming `nyl recover -e <env>`. A
lease that is already broken is reported as such. A holder that is in fact
still running can still cause effects, but it can no longer write state.

`nyl lease break -e dev --local` breaks the lease in the local repository
without contacting remote state.

## Options and exit codes

All commands take `-e/--environment`, `--reason`, `--wait-lease <duration>`
to wait for another run's lease, `--author "Name <email>"`, and `-o json` for
the operation's versioned report.

`--author` names the author and committer of the state commits the command
writes. Without it, Nyl uses Git's `user.name` and `user.email` for the
project's repository; without either, the command exits 1 with
`NYL-COMMIT-IDENTITY-MISSING` before changing anything. The commit time comes
from the same clock as the command's other time-based decisions.

They exit 0 when done or when nothing needed to change, 2 while another run
holds the lease (`NYL-LEASE-HELD`), a broken lease waits for its import
(`NYL-LEASE-BROKEN`), or `state delete --teardown` waits as `teardown` does,
and 1 for every other refusal, including `NYL-STATE-DELETED` on a deleted
location; each error
and warning carries a stable code and its fix. SIGINT or SIGTERM during
`--wait-lease` refuses with `NYL-RUN-CANCELLED` (exit 1) and writes nothing.

When `state delete --teardown` encounters manual units, it accepts the same
[approval flags](/nyl/reference/cli/reconcile/#options) as `reconcile`, including
`--approve-all`. Units requiring a reviewed destroy digest still need an
explicit `--approve <unit>=<digest>`.
