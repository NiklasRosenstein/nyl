---
title: 'teardown'
---

Tears units down on an operator's word: destroys what they created, through
their driver's teardown, and removes them from the environment's state.

```bash
nyl teardown -e dev --unit cache --reason "rebuild the corrupted cache"
nyl teardown -e dev --unit cache --hold --reason "incident 4711"
nyl teardown -e pr-123 --all --reason "pull request closed"
nyl teardown -e dev --unit kubernetes --confirm-removed --reason "workloads are gone"
```

`teardown` is explicit intent, so it needs no `--allow-teardown`. It acts on
the state the environment records and never renders source: each unit is
torn down from the desired document stored for it. It runs on the same path
as [`nyl reconcile`](/nyl/reference/cli/reconcile/): it takes the
environment's lease (and takes over a run whose lease was broken, importing
its results), tears units down in waves, consumers first, and writes one
transition commit per state ref with the operation `teardown` and the
`Nyl-Requested-By` and `Nyl-Reason` trailers.

## What it tears down

- `--unit <u>` (repeatable, or comma-separated): a unit **outside** the
  ownership set, such as one pending teardown or a `retained` tombstone,
  is torn down and its files are removed. A unit still **in** the
  ownership set is replaced: its incarnation is torn down, and the next
  `reconcile` creates it again with a new uid. Units that consume it keep
  their receipts and run again only when their own inputs change. A unit
  whose kind cannot tear down, such as a Command without a `teardown` step,
  is released: its resources stay and its state is removed, so a unit still
  in source is created fresh by the next `reconcile` unless `--hold` keeps
  it. Only the named units are touched: a named unit that a deleting unit
  still depends on waits for it, `pending-teardown`, exit 2.
- `--all`: every unit, consumers first. Units still in the ownership set are
  held afterwards, without an incarnation, so nothing recreates them.
  The hold records the supplied reason, or `teardown requested; automatic
  recreation is suspended` when none is supplied.
  `--all` honours `deletionPolicy: Retain`: Retain units and retained
  tombstones are released, and so are units whose kind cannot tear down,
  such as an `OciImage`. A released unit's resources stay in place and the
  transition commit lists what was left (`NYL-UNIT-RELEASED`).
- `--hold`: tear down, then hold, in the same operation, so no run can
  recreate the unit in between. The unit's desired file stays, held and
  without a uid, until [`nyl resume`](/nyl/reference/cli/hold/). When the
  teardown does not finish in the invocation, because it waits, failed, or is
  uncertain, the deleting unit is held where it stopped; after `nyl resume`
  the teardown continues, and the unit is held again once it completes, so
  it is not recreated until it is resumed again. An unfinished
  `--all` instead leaves its units deleting, so running it again or
  `--confirm-removed` continues it, and the run that completes a unit's
  teardown applies the hold.

```console
$ nyl teardown -e dev --unit worker --hold --reason "rotate the worker"
[dev/worker] starting Command (teardown)
[dev/worker] removed worker
[dev/worker] removed in 0.0s
UNIT    KIND     RESULT   DETAIL
worker  Command  removed

  run:      01a18581-0ea6-7282-9a50-7efce0a0c17e
  desired:  6eeccb9e675d2e8c6ae5cd1fcda3944afa50c191
  observed: f438cd48245c94da2a00a27bcb711dc4258914e0
```

A unit that needs a manual approval is reported `awaiting-approval` until
`--approve <unit>[=<digest>]` authorizes it. For `bind: plan`, obtain the
destroy digest with `nyl plan -e <env> --teardown --unit <unit> -o json`.
The [approval flags](/nyl/reference/cli/reconcile/#options) also work here:
`--approve-all`, `--approved-by`, and `--approval-source`. Approval does not
confirm workload removal; `--confirm-removed` is a separate claim.

A named unit the state does not hold exits 1 with `NYL-UNIT-UNKNOWN`.

### Held units

A held unit never progresses. A teardown of a held unit only records the
request on its held desired file, tears nothing down, and exits 0, reporting
the unit `held` with a pending teardown. After `nyl resume`, the next
`reconcile` or `teardown` carries the request out as an explicit teardown.

```sh
nyl teardown -e dev --unit config --reason 'replace config'
```

### Order and waits

A unit is torn down only after every deleting unit that depends on it. A
`KubernetesPublication` counts as torn down only once its `teardownWait`
completes, so the units it depends on wait with it. Units with no dependency
path to a waiting unit are torn down in the same run.

A unit that waits reports `pending-teardown` or `tearing-down` and the
command exits 2: a retained tombstone still cites it
(`NYL-TEARDOWN-HAS-DEPENDENTS`, overridden by `--allow-dependents`), a
publication's teardown waits (`NYL-TEARDOWN-WAITING`), or its teardown
readiness requirements are unmet. Running the command again continues where
it stopped.

## `--confirm-removed`

`--confirm-removed` ends a publication's teardown wait on the operator's
word, under any `teardownWait` strategy. When a run waits on the unit's
teardown, the confirmation is delivered to it through its signal ref: the
command reports `NYL-CONFIRMATION-DELIVERED`, exits 0, and starts no run of
its own; the waiting run finishes the teardown. When no run holds the lease,
the command runs the teardown itself and confirms the wait recorded in
state directly. A confirmation for a unit no teardown waits on exits 1 with
`NYL-CONFIRMATION-REJECTED` before anything runs, and a run doing anything
else keeps the lease (exit 2, `NYL-LEASE-HELD`). With `--all`, it confirms
every recorded wait. A confirmation never ends a wait that the same
command's phase 1 starts: that wait follows its strategy, and a later
`--confirm-removed` ends it.

A run that waits in a terminal also accepts Enter as the confirmation.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment. Required. |
| `--unit <u>` | The units to tear down. One of `--unit` and `--all` is required. |
| `--all` | Tear down every unit, then hold the units still in the ownership set. |
| `--hold` | Hold the units after their teardown. |
| `--confirm-removed` | Confirm that a publication's workloads are gone, ending its teardown wait. |
| `--allow-dependents` | Tear down units that retained tombstones still cite; the commit records the override. |
| `--allow-incomplete` | Proceed despite unmet readiness requirements or an unconfirmed removal, recording what may remain (`NYL-TEARDOWN-INCOMPLETE`). |
| `--fail-fast` | Start no further teardown after the first failure, uncertain outcome, or approval wait. What finished is still recorded in the transition commit. Without it, every unit that does not depend on a stopped one continues. |
| `--source <revision>` | Take the units' source files from this revision when their recorded source commit cannot be fetched; a teardown that cannot fetch it fails before any effect and names this option. Nyl warns and records the substitution under `sourceOverride`; it never refuses. |
| `--reason <text>` | Why, recorded as `Nyl-Reason`. |
| `--wait-lease <duration>` | Wait up to this long for another run's lease, such as `10m`. |
| `--author "Name <email>"` | The author of the state commits; defaults to Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table, or the typed `TeardownReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

## Results and exit codes

Each unit's result is one of `removed` (torn down, files removed),
`released` (resources left in place), `held` (a held unit; with a recorded
teardown request), `pending-teardown`, `tearing-down`, `failed`, or
`uncertain`.

| Exit | Meaning |
| --- | --- |
| 0 | Every named unit's deletion completed, or its request was recorded on a held unit; or a confirmation was delivered. |
| 1 | An error, such as `NYL-UNIT-UNKNOWN`, `NYL-CONFIRMATION-REJECTED`, or `NYL-STATE-DELETED`. |
| 2 | A teardown waits (dependents, a teardown wait, readiness, an approval), or another run holds the lease (`NYL-LEASE-HELD`). |
| 3 | A teardown failed. |
| 4 | A teardown is uncertain and needs [`nyl recover`](/nyl/reference/cli/recover/), the run lost its lease, or it was cancelled. |

## Codes

| Code | Fix |
| --- | --- |
| `NYL-UNIT-UNKNOWN` | Name units as `nyl get units -e <env>` lists them. |
| `NYL-TEARDOWN-HAS-DEPENDENTS` | Tear down or forget the retained tombstone first, or pass `--allow-dependents`. |
| `NYL-TEARDOWN-WAITING` | Wait for the publication's workloads to go, then run again, or confirm with `--confirm-removed`. |
| `NYL-TEARDOWN-INCOMPLETE` | Fix the readiness requirement it names, or accept what may remain with `--allow-incomplete`. |
| `NYL-UNIT-RELEASED` | Nothing to fix: the unit's resources were left in place, as the commit lists. Remove them by hand if they are no longer needed. |
| `NYL-CONFIRMATION-DELIVERED` | Nothing to fix: the waiting run completes the teardown; follow it with `nyl status -e <env>`. |
| `NYL-CONFIRMATION-REJECTED` | Check `nyl status -e <env>` for the unit whose teardown waits. |
| `NYL-LEASE-HELD` | Wait for the run to finish, or pass `--wait-lease`. |
| `NYL-UNIT-HELD` | A held unit only recorded the teardown request. Lift the hold with `nyl resume`. |
| `NYL-RUN-STOPPED` | `--fail-fast` stopped the teardown after a unit failed, became uncertain, or waited for an approval. |
| `NYL-SOURCE-UNAVAILABLE` | The unit's recorded source commit cannot be fetched. Fetch it, or pass `--source <revision>`. |
