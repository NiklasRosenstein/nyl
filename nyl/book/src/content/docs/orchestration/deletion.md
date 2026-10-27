---
title: 'Deletion and holds'
---

Units leave an environment when source stops declaring them, when the
Environment no longer selects them, or when an operator tears them down. Nyl
never destroys anything by accident: a unit that leaves the ownership set
waits for an explicit `--allow-teardown`, units are torn down consumers
first, and a unit that should keep its resources leaves a tombstone instead.
This page explains how a run deletes units, how
[`nyl teardown`](/nyl/reference/cli/teardown/) tears them down on request,
and how [`nyl hold`](/nyl/reference/cli/hold/) freezes a unit during an
incident.

## The lifecycle of a unit

Every desired document in state has a `lifecycle` block. A unit's whole
history is the diff of one file, and the last desired document a teardown
needs stays in it:

```yaml
lifecycle:
  state: deleting                    # active | deleting | held | retained
  deletion:
    reason: omission                 # omission | teardown | replace | kind-change | decommission
    intent: teardown                 # retain | teardown
    teardown: pending                # pending | in-progress
    requested: {by: alice, at: 2026-09-24T12:00:00Z, reason: decommission}
```

`nyl status` shows the states as `pending-teardown` (deleting, teardown not
started), `tearing-down` (the driver's teardown started, such as a
publication waiting under its `teardownWait`), `held`, and `retained`.

## Leaving the ownership set

A unit leaves the ownership set when it is removed from source, deselected by
the Environment's `unitSelector`, or disabled with `enabled: false`. What
happens next depends on its `deletionPolicy`:

| Policy | Default for | When it leaves |
| --- | --- | --- |
| `Teardown` | Kinds that can tear down, such as a Command with a `teardown` step | It becomes `pending-teardown`; `nyl reconcile --allow-teardown` tears it down and removes its files. |
| `Retain`, declared | Units that declare it | It becomes a `retained` tombstone: its resources stay, its last desired document, uid, and receipt are kept in state. |
| `Retain`, by kind | Kinds that cannot tear down, such as a Command without a `teardown` step | It is released: its resources stay in place and its files are removed. The transition commit lists what was left. |

A teardown caused by a change to source needs `--allow-teardown`. Without it
the unit stays `pending-teardown`, keeping its uid and receipt, and the run
exits 2 with `NYL-TEARDOWN-PENDING`. A selector typo therefore destroys
nothing, and fixing it returns the unit to `active` with its receipt, so it
does not run again. [`nyl plan`](/nyl/reference/cli/plan/) lists leaving
units first, with the destroy plan `--allow-teardown` would carry out.

Here `cache` (a Command with a `teardown` step) and `worker` (which reads
`cache`'s endpoint and declares `deletionPolicy: Retain`) were removed from
source:

```console
$ nyl reconcile -e dev
Units leaving the ownership set of dev (2):
  teardown, needs --allow-teardown:   cache (removed)
  dropped from state, resources kept: (none)
  retained as tombstones:             worker (removed)

UNIT    KIND     RESULT            DETAIL
cache   Command  pending-teardown
worker  Command  retained
run: 01a17a82-ac26-7de8-923e-533f84f9cfae
source: a16f93e6a1b69c767af8a3f37c84e6f102c20858
desired: fe129b91809ec6961f7b32453bc6fcc2239200cb
observed: 6b48875629cd176dc861c5a779a0562466174463
warning NYL-PLAN-UNITS-LEAVING: 2 unit(s) leave the ownership set of dev: cache (removed), worker (removed)
  fix: restore the units in source or in the unitSelector's labels if they should stay; pass --fail-on-leaving to plan to make removals fail a pipeline
warning NYL-TEARDOWN-PENDING (cache): unit cache of dev left the ownership set; its teardown waits for --allow-teardown
  fix: pass --allow-teardown to nyl reconcile to tear it down, restore it in source to keep it, or let it go without a teardown with `nyl state forget -e dev --unit cache`
```

`--allow-teardown` gates only the start of a teardown. A teardown that
started and failed, or that waits, continues in the next run without it, and
so does a teardown an operator requested with `nyl teardown`.

`nyl reconcile --leaving` acts only on the units outside the ownership set;
`nyl reconcile --unit` does no deletion work at all.

## Order

Teardown follows the reverse of creation order: a unit is torn down only
after every deleting unit that depends on it is gone, so consumers go first
and producers follow once nothing uses them. Retentions and releases follow
the same rule, so a producer's tombstone is written only once its deleting
consumers are torn down. A unit with no dependency path to a unit that waits
is torn down in the same run.

## Retained tombstones

A `retained` tombstone keeps running resources that may still use their
producers. Tearing down a unit that a tombstone's receipt cites refuses with
`NYL-TEARDOWN-HAS-DEPENDENTS` (exit 2) unless the invocation passes
`--allow-dependents`; the transition commit records the override under
`allowDependents`.

`worker`'s tombstone cites `cache`, so tearing `cache` down needs both
overrides:

```console
$ nyl reconcile -e dev --allow-teardown --allow-dependents
removing the cache
Units leaving the ownership set of dev (1):
  teardown, needs --allow-teardown:   cache (removed)
  dropped from state, resources kept: (none)
  retained as tombstones:             (none)

UNIT    KIND     RESULT    DETAIL
cache   Command  removed
worker  Command  retained
run: 01a17a82-ae30-7f9b-a80a-cd3d9d972431
source: a16f93e6a1b69c767af8a3f37c84e6f102c20858
desired: 8baf76b5b32791f571720e1c49d39a5c8c721c7c
observed: d06d598d244736f6e75ad24cb86688643990ca95
warning NYL-PLAN-UNITS-LEAVING: 1 unit(s) leave the ownership set of dev: cache (removed)
  fix: restore the units in source or in the unitSelector's labels if they should stay; pass --fail-on-leaving to plan to make removals fail a pipeline
```

A unit that returns with the same name and kind adopts its tombstone. Its
execution key alone decides: when the key is unchanged, the tombstone's
receipt is current at once and nothing runs again, even when a producer was
recreated with a new uid in the meantime. Otherwise the unit runs as an
ordinary change of the same incarnation. A unit that must run again whenever
its producer is recreated says so in its inputs, for example by consuming an
output that changes with every creation.

Restoring both units afterwards creates a new `cache` with the same endpoint,
and `worker` adopts its tombstone without running:

```console
$ nyl reconcile -e dev
UNIT    KIND     RESULT    DETAIL
cache   Command  executed
worker  Command  current
run: 01a17a82-b014-7cfa-a9de-b8fdecc75ea5
source: 2ad296eaa4add7a001d5a62229d25ec3d55ada63
desired: a87cceab5ea1807e3c4c2c0f3d3fe33ea7ce5f0e
observed: aee94e95faa5fcf4e6076c42ad64a4e66ffe324e
```

[`nyl state forget`](/nyl/reference/cli/state/) drops a tombstone or a
`pending-teardown` unit from state without touching its resources.

## Kind changes

Changing a unit's `apiVersion` or `kind` under the same name ends the old
incarnation: it becomes deleting with `reason: kind-change`, which needs
`--allow-teardown` like any omission, and the run warns with
`NYL-UNIT-KIND-CHANGE`. The new kind waits until the old incarnation's file
is removed, then starts with a new uid; a deleting incarnation of any kind
fences a new one the same way. A retained tombstone of another kind is
dropped with its resources left in place.

## Explicit teardown

[`nyl teardown -e <env> --unit <u>`](/nyl/reference/cli/teardown/) tears a
unit down whether or not it is in the ownership set. The command is explicit
intent, so it needs no `--allow-teardown`. It acts on the documents in state
and renders nothing: each teardown runs with the files of the unit's
recorded `sourceRevision`. When that commit can no longer be fetched, the
teardown fails before any effect with a fix naming `--source <revision>`,
which runs it with another revision's files and records the override in the
transition commit.

- A unit outside the ownership set (a deleting unit, or a tombstone) is torn
  down and its files are removed.
- A unit still in the ownership set is replaced (`reason: replace`): its
  incarnation is torn down, and the next `reconcile` creates a new one with a
  new uid. Dependents resolve against the new receipt's outputs and run only
  when their execution key changes.
- `--hold` holds the unit in the same operation, so no run recreates it
  between the teardown and a separate `nyl hold`: its desired file stays,
  held, without a uid.
- `--all` tears down every unit, dependents first, and holds the units of the
  ownership set afterwards. It honours `deletionPolicy: Retain`: declared
  `Retain` units, retained tombstones, and kinds that cannot tear down are
  released, keeping their resources, and listed under `released` in the
  transition commit.

A teardown that fails leaves the teardown in progress with a `failed`
condition (exit 3), and one whose outcome is unknown, or that passes the
unit's `timeout`, leaves it `uncertain` (exit 4). The kind's teardown
recovery policy decides what happens next, as for executions: an idempotent
teardown runs again in the next run, and anything else waits for
[`nyl recover`](/nyl/reference/cli/recover/).

## Holds

[`nyl hold -e <env> --unit <u>`](/nyl/reference/cli/hold/) makes runs change
nothing about a unit, whatever state it is in, without touching its
resources. A held unit never progresses:

- A running incarnation keeps the desired document its receipt was produced
  for, so its consumers keep resolving against what actually runs. Runs
  record what source would change under `hold.pending`, and write the held
  file only when that changes.
- A unit without an incarnation, including one that was never reconciled in
  the environment, is not created; its dependents stay blocked.
- A deleting unit pauses where it is.
- A teardown of a held unit only records the request on the held file and
  tears nothing down; the run reports the unit `held` and exits 0. After
  `nyl resume`, the next `reconcile` or `teardown` carries the request out.
- A held unit that leaves the ownership set is reported as held with a
  pending deletion. After `nyl resume`, a held file without an incarnation
  outside the ownership set is removed by the next reconcile.

`nyl hold` also works while a run holds the environment's lease: the run
reads the hold before it starts each unit, starts no held unit, and writes
the hold in its own transition commit. Holds are checked between teardown
batches. Holding a publication during its teardown wait pauses it before
phase two; its recorded wait stays in state until you resume it.

## Publication teardown waits

A `KubernetesPublication` unit tears down in two phases: it publishes the removal, waits under its
`teardownWait` until Argo CD has removed the workloads, and only then
removes the rest. While it waits, the units it depends on wait with it,
reported with `NYL-TEARDOWN-WAITING`, and units with no path to it proceed.
Before a publication's teardown starts, Nyl checks that its target's settings
let the teardown complete; an unmet requirement keeps the unit
`pending-teardown` unless the invocation passes `--allow-incomplete`, and a
teardown completed without a claim of removal is recorded under `incomplete`
with `NYL-TEARDOWN-INCOMPLETE`.

## Codes

| Code | Meaning and fix |
| --- | --- |
| `NYL-TEARDOWN-PENDING` | A teardown waits: for `--allow-teardown`, for the deleting units that depend on it, or for an approval. Pass `--allow-teardown`, restore the unit in source, or drop it with `nyl state forget`. |
| `NYL-TEARDOWN-HAS-DEPENDENTS` | Retained tombstones cite the unit. Pass `--allow-dependents`, or forget the tombstones first. |
| `NYL-TEARDOWN-WAITING` | A publication it depends on waits under its `teardownWait`. Run again once the wait is over, or confirm the removal with `nyl teardown --confirm-removed`. |
| `NYL-TEARDOWN-INCOMPLETE` | The teardown cannot complete, or completed without a claim of removal. Meet the requirements, or pass `--allow-incomplete` and check what remains. |
| `NYL-UNIT-RELEASED` | A unit's resources were left in place. Remove them by hand when nothing needs them. |
| `NYL-UNIT-KIND-CHANGE` | A unit changed its kind; the old incarnation is deleting. Pass `--allow-teardown`, restore the old kind, or forget the old incarnation. |
| `NYL-UNIT-HELD` | A held unit keeps changes or a teardown from applying. Lift the hold with `nyl resume`. |

## See also

- [Runs](/nyl/orchestration/runs/), for how a run executes units.
- [Recovery](/nyl/orchestration/recovery/), for `uncertain` and failed units.
- [`nyl teardown`](/nyl/reference/cli/teardown/), [`nyl hold`](/nyl/reference/cli/hold/),
  and [`nyl reconcile`](/nyl/reference/cli/reconcile/).
