---
title: 'hold and resume'
---

`nyl hold` freezes a unit: `reconcile` changes nothing about it, whatever
state it is in, without touching its resources. `nyl resume` lifts the hold.
A hold is for incidents and investigations, to stop reconciling a unit while
something is wrong without a source change and its review cycle. It is the
only desired state that does not come from source, and it is an explicit,
recorded operator action; long-term changes belong in source.

```bash
nyl hold -e dev --unit database --reason "incident 4711"
nyl resume -e dev --unit database --reason "incident 4711 resolved"
```

Each writes one desired commit with the operation `hold` or `resume`, the
unit and its action under `units`, a headline such as
`nyl: hold dev (database)`, and the `Nyl-Requested-By` and `Nyl-Reason`
trailers. Holding a held unit, or resuming a unit that is not held, changes
nothing, writes no commit, and exits 0 with a note, so scripts can repeat
them. `nyl hold` never renders source.

## What a hold keeps

- **A unit with a running incarnation** keeps the desired document its
  latest receipt was produced for, with its uid. Its receipt stays current
  against that document, so units that consume it keep resolving from what
  actually runs. Newer source the hold keeps from applying is recorded under
  `hold.pending`, and `nyl status` shows it as "pending changes from
  <commit>".
- **A unit with an `uncertain` condition** is held the same way: its desired
  state is the document and uid of its last receipt, and the condition and
  any evidence ref stay. `status` shows the unit as `held (uncertain)` and
  exits 4, and `resume` does not clear the condition: the unit still needs a
  run under its recovery policy or [`nyl recover`](/nyl/reference/cli/recover/).
  An uncertain unit without a receipt has nothing to fall back on, and its
  dependents stay blocked.
- **A unit that never ran**, or one nothing shows ever ran, is held without
  an incarnation, so it is not created while held and its dependents stay
  blocked. This includes a unit the environment has never reconciled:
  `nyl hold` writes a held marker, a desired file with
  `lifecycle.state: held` and no uid, kind, or rendering.
- **A deleting unit** pauses where it is: no retention or teardown happens,
  and its file keeps its `deletion` block next to `hold`.
- **A held unit that leaves the ownership set** is not deleted; it is
  reported held with a pending deletion.

A `retained` tombstone cannot be held: nothing reconciles it. `nyl hold`
exits 1 with `NYL-HOLD-REFUSED`, naming
[`nyl teardown --unit`](/nyl/reference/cli/teardown/) and
[`nyl state forget`](/nyl/reference/cli/state/#nyl-state-forget).

A [`nyl teardown`](/nyl/reference/cli/teardown/) of a held unit only records
the request on its held file; the unit is torn down by the first
`reconcile` or `teardown` after `resume`.

## Resume

`nyl resume` only lifts the hold: it deletes nothing and keeps the unit's
uid, receipt, and condition. A held unit with a deletion returns to
`deleting` where it stopped, and a recorded teardown request is carried out
by the next `reconcile` or `teardown`. The next `reconcile` renders the unit
from source again and applies whatever changed: a unit without an
incarnation, such as a held marker, starts a new one, and one that is no
longer in the ownership set has its file removed.

```console
$ nyl hold -e dev --unit migrate --reason "incident 4711"
hold dev: done
  desired: 007d16bb3c4b32aadd06ea7882629e1d48b5a716
  marker: true
  unit: migrate
$ nyl status -e dev
UNIT     KIND  STATE  RECEIPT  LAST RUN  DETAIL
migrate  -     held   none     -
warning NYL-UNIT-HELD (migrate): migrate is held by Alice <alice@example.com> (incident 4711) since 2026-10-26T23:03:58Z
  fix: reconcile changes nothing about it until `nyl resume -e dev --unit migrate`
```

## While a run holds the lease

`nyl hold` does not wait for a running run. It delivers the hold to the run
through the run's signal ref, reports `NYL-HOLD-DELIVERED` with the run and
runner under `details.deliveredTo`, and exits 0 without a commit of its
own. The run checks its signals before starting each unit, starts no held
unit, writes the hold in its own transition commit (listed under `held`),
and reports the unit as held. If the run finished before it read the
signal, `nyl hold` removes the signal and holds the unit itself.

`nyl resume` takes the lease like every state operation: while a run holds
it, `resume` exits 2 with `NYL-LEASE-HELD`; `--wait-lease <duration>` waits
for it instead.

A lease broken with [`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break)
whose run is not imported yet stops both with `NYL-LEASE-BROKEN` (exit 2):
`nyl recover -e <env>` imports the run first.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment. Required. |
| `--unit <name>` | The unit to hold or resume. Required. |
| `--reason <text>` | Why, recorded in the hold and as `Nyl-Reason`. |
| `--wait-lease <duration>` | Wait up to this long for another run's lease, such as `10m`. |
| `--author "Name <email>"` | The author of the state commit; defaults to Git's `user.name` and `user.email`. |
| `-o`, `--output <text\|json>` | Lines, or the typed `StateOperationReport`. Default `text`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Done, delivered to the running run, or nothing to change. |
| 1 | An error or refusal, such as `NYL-HOLD-REFUSED`, `NYL-UNIT-UNKNOWN` (`resume` of a unit the state does not hold), or `NYL-STATE-DELETED`. |
| 2 | The lease is broken and its run not imported (`NYL-LEASE-BROKEN`), or, for `resume`, another run holds it (`NYL-LEASE-HELD`). |

## Codes

| Code | Fix |
| --- | --- |
| `NYL-HOLD-REFUSED` | A retained tombstone: tear it down with `nyl teardown --unit`, or drop it with `nyl state forget`. |
| `NYL-HOLD-DELIVERED` | Nothing to fix: the running run records the hold; follow it with `nyl status -e <env>`. |
| `NYL-UNIT-HELD` | A warning of `status`, `plan`, and `reconcile`: reconcile changes nothing about the unit until `nyl resume`. |
| `NYL-LEASE-BROKEN` | Import the broken run with `nyl recover -e <env>`, then run the command again. |
| `NYL-LEASE-HELD` | Wait for the run to finish, or pass `--wait-lease`. |
