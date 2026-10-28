---
title: 'Recovery'
---

Runs die: a CI job is killed, a runner loses its network, or someone cancels a
pipeline. Orchestration never loses a result it recorded and never guesses one
it did not. This page explains how the next run takes over from a run that
died, what an `uncertain` unit is, and how
[`nyl recover`](/nyl/reference/cli/recover/) and
[`nyl verify`](/nyl/reference/cli/verify/) help an operator settle it.

## Leases, run refs, and checkpoints

Every run takes the environment's lease before it changes anything, so one
run at a time changes an environment. Next to the lease, it keeps a run ref,
`refs/nyl/<environment>/runs/<run>` under the default
`coordinationRefPrefix`, with its `Run` record: the units it is executing and
a deadline. Each phase moves the deadline: the resolution timeout (30 minutes)
for resolution, the longest `timeout` of the units starting for a wave, plus a
10-minute grace period either way. Before a unit the run creates starts, the
run records its desired document and new uid on the run ref; when a unit
finishes, the run checkpoints its receipt or failure there before the next
wave. [Git storage](/nyl/reference/git-storage/#coordination-refs) lists every
file on these refs.

While a run is alive, its lease and its run ref both hold the environment:

```console
$ nyl reconcile -e dev
error NYL-LEASE-HELD: dev is held by run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 (reconcile) by unknown@localhost, until 2026-10-25T13:19:59.431883317+00:00
  fix: wait for that run to finish, pass --wait-lease <duration>, or run `nyl lease break` if its runner is gone
```

`--wait-lease <duration>` waits for the lease instead of exiting 2. Deleting
the lease ref alone does not let a second run in, because the run ref still
carries the deadline; the holder exits 4 with `NYL-LEASE-LOST` at its next
lease update and pushes nothing to the state refs.

## Taking over

A run that finds the lease expired, or [broken](#breaking-a-lease), replaces
it and takes over from every run ref left behind. A broken lease is taken
over only by an operation that imports the broken run: `nyl reconcile`,
`nyl teardown`, or `nyl recover -e <env>`, which imports without
executing.

- It reuses the uid of every unit a dead run created and started, so a
  resource that run created always has state, even when the unit is no longer
  selected.
- A checkpoint whose execution key equals the unit's key in the new
  resolution becomes the unit's receipt. The unit is listed under `imported`.
- A unit the dead run was executing without a checkpoint gets an `uncertain`
  condition naming the dead run. The unit is listed under `recovered`.
- A checkpoint with another key is superseded (`NYL-RUN-SUPERSEDED`). When it
  recorded real effects, the unit becomes `uncertain` too, whatever its kind.

Importing is idempotent: a second run that reads the same run ref changes
nothing, and a checkpoint never overwrites a newer result. After its
transition commit lands, the run deletes the run refs it consumed.

Here a runner was killed while `migrate` ran, after `database` was
checkpointed. Its lease was broken, and the next run imports `database` and
marks `migrate` uncertain:

```console
$ nyl reconcile -e dev
UNIT      KIND     RESULT     DETAIL
database  Command  imported
migrate   Command  uncertain  run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 was executing it when its lease was broken
run: 01a1732a-44de-7b8d-90ca-4602a61d3a51
source: 8400718ac2236aa6b71cd5a9c43d4ddd94545257
desired: fcfbb6fb6d03c2c3a539c4ef19787d50a9614d43
observed: e83e5e124d593a94226d7a3287df158b2c87e93f
```

Its transition commit records both lists and the break:

```text
nyl: reconcile dev (1 imported, 1 uncertain)

units:
  database: {"executionKey":"sha256:6916707da65148797708b9f4754dbf1e9fe43eb229a2a6e5db09b933c3b6611f","result":"imported","run":"01a1732a-11d1-7ed6-a3cc-174dc69c7856"}
  migrate: {"reason":"run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 was executing it when its lease was broken","result":"uncertain"}
recovered: [migrate]
imported: [database]
leaseBroken: {"at":"2026-10-25T13:05:12.386099409Z","by":"Alice Example <alice@example.com>","reason":"runner was killed"}
```

## Uncertain units and recovery policies

An `uncertain` condition means effects may or may not have happened: a run
died during the execution, the execution passed its `timeout`, or the run
was cancelled. The condition keeps the unit's last receipt and makes the unit
a candidate in the next run whatever that receipt says. What happens then is
its kind's recovery policy:

| Policy | Next run | Kinds |
| --- | --- | --- |
| `converge` | Executes the unit again, in the same run that marked it. The result is `recovered`. | [Command](/nyl/orchestration/resource-guides/command/) with `idempotent: true`, and the planned OpenTofu, OciImage, and KubernetesPublication kinds |
| `inspect` | Asks the driver whether the effects were applied: applied records the receipt (`recovered`), not applied executes again, and unknown leaves the condition for `nyl recover` | Kinds that can observe their effects |
| `manual` | Leaves the condition; the unit waits for `nyl recover` | Command without `idempotent: true` |

`nyl status` reports uncertain units with `NYL-UNIT-UNCERTAIN` and exits 4:

```console
$ nyl status -e dev
UNIT      KIND     STATE      RECEIPT  LAST RUN                              DETAIL
database  Command  current    current  01a1732a-11d1-7ed6-a3cc-174dc69c7856
migrate   Command  uncertain  none     01a1732a-11d1-7ed6-a3cc-174dc69c7856  run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 was executing it when its lease was broken
source: 8400718ac2236aa6b71cd5a9c43d4ddd94545257
warning NYL-UNIT-UNCERTAIN (migrate): unit migrate is uncertain: run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 was executing it when its lease was broken
  fix: check what the execution did; a unit whose recovery policy converges runs again in the next nyl reconcile, otherwise run `nyl recover -e dev --unit migrate --retry --reason <text>` once it is safe to run it again
```

Failures are different: a failure claims the execution made no effect it did
not report. A retryable failure runs again in the next reconcile; a
non-retryable one waits for `nyl recover --retry`.

## Recovering a unit

[`nyl recover`](/nyl/reference/cli/recover/) records an operator's decision
about one unit, with a reason, in one observed commit. It never executes
anything; the next reconcile does, so you can decide on your workstation
while CI executes.

`--retry` clears the condition, uncertain or failed, and keeps the last
receipt. Use it once you have checked that running the unit again is safe:

```console
$ nyl recover -e dev --unit migrate --retry --reason "migration not applied, checked the schema version"
recover dev: done
  observed: eefebb83f90d3ad19b04b6f1b9cf84419aefbd85
  condition: {"reason":"run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 was executing it when its lease was broken","run":"01a1732a-11d1-7ed6-a3cc-174dc69c7856","since":"2026-10-25T13:05:12.459621639Z","state":"uncertain"}
  evidence: []
$ nyl reconcile -e dev
UNIT      KIND     RESULT    DETAIL
database  Command  current
migrate   Command  executed
run: 01a1732a-5626-7e0b-8be9-1f86f45686fb
source: 8400718ac2236aa6b71cd5a9c43d4ddd94545257
observed: a86335501a1f0e3b0dd7e244d382ebd411adf6fd
```

`--accept <run>` adopts what a run checkpointed for the unit, its receipt,
outputs, and artifacts, as the unit's receipt, when the checkpoint's execution
key and uid equal the unit's current desired document's. Accepting an
execution as applied needs its outputs, so a checkpoint is the only evidence
`recover` accepts. The `evidence` that `recover` prints lists every checkpoint
of the unit Nyl still holds, newest run first, and whether `--accept` would
adopt it.

A unit held with `nyl hold` keeps the desired document of its last receipt,
so its consumers keep resolving from that receipt while it is investigated.
Its condition stays, and `nyl resume` does not clear it. `--retry` on a held
unit clears only the condition; the unit stays held. `--accept` refuses a
held unit, which runs nothing a checkpoint could match: resume it first.

## Late checkpoints and evidence refs

A runner whose lease was broken may still be alive and finish its unit after
another run marked that unit uncertain. Its late checkpoint never clears the
condition by itself: the next run reports it with `NYL-RUN-LATE-CHECKPOINT`
and keeps it as evidence. A taking-over run retires a run ref whose
checkpoints an uncertain unit still needs to an evidence ref,
`refs/nyl/<environment>/evidence/<run>`, instead of deleting it. Evidence refs
are never imported again and never hold the lease. `nyl recover --accept
<run>` adopts the late checkpoint, and `recover` deletes evidence refs once no
uncertain unit needs them.

## Cancellation

On SIGINT or SIGTERM, `nyl reconcile` starts no further unit and forwards the
signal to the running tools' process groups, so a tool can stop cleanly and
release its own locks; a tool still running 30 seconds later is killed. On
Windows, which has no process groups to signal, the console delivers Ctrl+C
to the tool directly, and Nyl kills it after the same 30 seconds. The
run checkpoints the units that finish, gives the ones still running an
`uncertain` condition, marks its run ref finished, releases the lease, and
exits 4 with `NYL-RUN-CANCELLED`, without a transition commit. The next run
takes over at once, as after a crash. Until then, `nyl status` reports the
cancelled run with `NYL-RUN-UNIMPORTED` and the units it left uncertain.

CI systems allow only seconds between the signal and a forced kill, and a
killed run leaves its lease until the deadline. Do not cancel pipelines that
apply changes: a CI concurrency group per environment serializes jobs
without cancelling them.

## Breaking a lease

When a holder died without releasing its lease, for example after a forced
kill, waiting for its deadline can take an hour.
[`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break) declares it
gone:

```console
$ nyl lease break -e dev --reason "runner was killed"
lease-break dev: done
  broken: run 01a17a9c-0455-77cd-a40d-42e3b92482da
warning NYL-LEASE-HELD: the lease of run 01a17a9c-0455-77cd-a40d-42e3b92482da by unknown@localhost was broken; if that run is still alive it may still cause effects, but it can no longer write state
  fix: make sure the runner is gone; units it was executing become uncertain at the next run
```

A holder that is in fact alive can no longer write state, because every
transition commit is pushed together with a check of the holder's own lease.

The dead run's effects must be recorded before anything else writes state.
So only an operation that imports the broken run takes the broken lease
over: `nyl reconcile`, `nyl teardown`, or `nyl recover -e <env>`. Every other
command that writes state, `nyl verify`, `nyl hold`, and the `nyl state`
commands included, exits 2 with `NYL-LEASE-BROKEN`, or waits under
`--wait-lease`:

```console
$ nyl state forget -e dev --unit migrate
state-forget dev: refused
error NYL-LEASE-BROKEN: the lease of dev was broken and its run 01a17a9c-0455-77cd-a40d-42e3b92482da has not been imported yet
  fix: run `nyl recover -e dev` to import it (a `nyl reconcile` or `nyl teardown` of dev imports it too), then run this command again
```

`nyl recover -e <env>` imports the run as a reconcile takeover would, with
execution switched off: units the run was executing become `uncertain`, and
the next reconcile runs them under their recovery policy. It writes one
transition commit and no run record of its own, and exits 0 once the import
is recorded:

```console
$ nyl recover -e dev --reason "import the killed run"
recover dev: done
  desired: f99b8341625ba6f6fccae39f32651c71b9cd518c
  observed: 1547cfcc32bd21a1c3905e940a421d32b318e463
  imported: []
  leaseBroken: {"at":"2026-10-26T23:46:51.527951350Z","by":"Alice Example <alice@example.com>","reason":"runner was killed"}
  recovered: ["migrate"]
```

Whichever operation imports the run records the break once, under
`leaseBroken` with who broke the lease and why, next to the import, and
consumes the broken run's refs. It records the break even when the run left
nothing to import, such as a broken `nyl hold`. An importer that ends before
its transition commit lands, such as a reconcile that fails during
resolution or is cancelled, puts the broken lease back when it releases, so
the break stays in effect until an import is committed.

When the state cannot be imported into, because `state.yaml` is missing
after a broken `nyl state init`, the state moved away, or a remote tip is a
`Nyl-Local` commit, `nyl recover -e <env>` still takes the broken lease over
and releases it, so `nyl state init`, `nyl state move`, or `nyl state
restore` can repair the state. See [`nyl recover`](/nyl/reference/cli/recover/#importing-a-broken-run).

## Verifying receipts

[`nyl verify -e <env>`](/nyl/reference/cli/verify/) asks each unit's driver
whether what its current receipt describes still holds, without executing
anything. It records the latest drift observation, a health observation when
the driver reports one, and the attestations the driver reports, in one
observed commit, and writes nothing when nothing changed. It never changes a
receipt or a condition; `verify` exits 3 on drift so that a pipeline can
alert on it.

## Repairing drift

A unit whose latest drift observation is `drifted` is `drifted` in
[`nyl status`](/nyl/reference/cli/status/), which exits 2 for it, and
[`nyl plan`](/nyl/reference/cli/plan/) lists it as a unit that would execute:
like a changed unit, it is work for the next reconcile.

- With the Environment's `driftRepair: converge`, the default, the next
  `nyl reconcile` executes every drifted unit whose recovery policy is
  `converge` again, and its transition commit lists it under `driftRepaired`.
  The new receipt removes the drift observation in the same commit, so the
  unit is not repaired again until `verify` finds new drift.
- A drifted unit that is not safe to re-run is reported `awaiting-approval`
  (exit 2) while the rest of the run continues. A matching
  `--approve <unit>` executes it and records the approval's identity.
- `driftRepair: manual`, on the Environment or its EnvironmentGroup, turns
  automatic repair off: drifted units stay `drifted` until a forced run.
- `nyl reconcile -e <env> --unit <u> --reapply --reason <text>` executes the
  named units again although their receipts are current, drifted or not, and
  records who asked and why under `reapplied`. A named unit that is not safe
  to re-run follows the approval rule above.

`nyl verify`'s `NYL-VERIFY-DRIFT` warning says for each unit what the next
reconcile does with it.

## Recovery next to local runs

A [`--local` run](/nyl/orchestration/environments-and-state/#local-runs)
never takes over remote runs: while remote state holds dead runs, it exits 1
with `NYL-LOCAL-RUN-LEFTOVERS`, and a remote `nyl reconcile` or `nyl recover
-e <env>` takes them over first. Under the remote lease, a `--local` run also
refuses a broken remote lease with `NYL-LEASE-BROKEN` (exit 2), even when the
broken holder left no run ref, because only an import records the break. A `--local` run executes a unit whose recovery policy is not
`converge` only when named with `--approve <unit>`.

## See also

- [Runs](/nyl/orchestration/runs/)
- [Environments and state](/nyl/orchestration/environments-and-state/)
- [Git storage](/nyl/reference/git-storage/)
- [`nyl reconcile`](/nyl/reference/cli/reconcile/), [`nyl recover`](/nyl/reference/cli/recover/), [`nyl verify`](/nyl/reference/cli/verify/), [`nyl status`](/nyl/reference/cli/status/)
