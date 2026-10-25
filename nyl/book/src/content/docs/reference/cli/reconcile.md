---
title: 'reconcile'
---

Executes an environment's ready units in dependency order and records the
results in its state. See [Runs](/nyl/orchestration/runs/) for how a run
chooses its source commit, resolves references, and decides what executes.

:::note
Orchestration is being built in milestone M3. `reconcile` runs one
environment's [Command](/nyl/orchestration/resource-guides/command/) units and
takes over from runs that died; teardown, holds, remote approvals, and runs
over several environments follow.
:::

```bash
nyl reconcile -e dev
nyl reconcile -e dev --unit migrate
nyl reconcile -e dev --units database,migrate --concurrency 2
nyl reconcile -e prod --source 3e7b9c1 --author "Release Bot <release@example.com>"
nyl reconcile -e dev -o json
nyl reconcile -e dev --wait-lease 30m
nyl reconcile -e dev --local --approve migrate
```

The environment's state must exist: create it once with
[`nyl state init`](/nyl/reference/cli/state/). The run takes the environment's
lease, executes waves of ready units until nothing is ready, writes one
transition commit per state ref it changes, and releases the lease. A run
that changes nothing writes no commit.

## Takeover

When the lease has expired, or was broken with
[`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break), the run
replaces it and imports the run refs earlier runs left behind, as
[Recovery](/nyl/orchestration/recovery/#taking-over) describes: a
checkpoint with the unit's current execution key becomes its receipt
(`imported`), a unit a dead run was executing without a checkpoint becomes
`uncertain` (`recovered`), and a converging uncertain unit executes again in
the same run. The transition commit lists both under `imported` and
`recovered`. While a live run holds the lease or its run ref still carries a
deadline, the run exits 2 with `NYL-LEASE-HELD`, unless `--wait-lease` waits
for it.

## Cancellation

SIGINT or SIGTERM cancels the run: it starts no further unit, forwards the
signal to running tools, and kills a tool still running 30 seconds later. It
checkpoints the units that finish, marks the ones still running
`uncertain`, keeps its run ref for the next run to take over, releases the
lease, writes no transition commit, and exits 4 with `NYL-RUN-CANCELLED`.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment to reconcile. Required. |
| `--unit <name>`, `--units <a>,<b>` | Execute only these units. Resolution still covers the whole environment, and a named unit whose dependency has no current receipt stays blocked. An unknown name exits 1 with `NYL-UNIT-UNKNOWN`. |
| `--concurrency <n>` | Units a wave executes at once. Default 4. |
| `--source <rev>` | Render units at this revision or commit instead of the Environment's source, for this run only. It must pass the same [reachability checks](/nyl/orchestration/runs/#source-commit), and the transition commit marks it as an override. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease, instead of exiting 2 with `NYL-LEASE-HELD`. |
| `--local` | Run next to remote state without publishing it: continue from this clone's [local state branches](/nyl/orchestration/environments-and-state/#local-runs) and commit there, marked `Nyl-Local: true`. Warns `NYL-LOCAL-RUN-UNRECORDED` for every unit it executes. Exits 1 with `NYL-LOCAL-RUN-UNSUPPORTED` for state without a remote or with full `refs/…` state refs, and with `NYL-LOCAL-RUN-LEFTOVERS` while remote state has dead runs to take over. |
| `--local-lease` | With `--local`, take this clone's own lease instead of the remote one, accepting that CI may run at the same time. |
| `--approve <unit>` | With `--local`, execute this unit although its recovery policy is not `converge`. Its receipt and the local transition commit record `approval: {by: [<commit identity>], identity: local, source: local, digest: null, reviewed: false}`. Repeatable or comma-separated. Other such units wait as `awaiting-approval`, exit 2. Only `--local` accepts it until remote approvals arrive. |
| `--author "Name <email>"` | The author of the state commits. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table of units, or the typed `ReconcileReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

The table lists each unit with its result: `executed`, `current`,
`imported`, `recovered`, `failed`, `uncertain`, `blocked`, or
`awaiting-approval`, as [Runs](/nyl/orchestration/runs/#transition-commits)
defines them. `-o json` prints the same
report as JSON on stdout, with `result` per unit as in the transition commit
summary; diagnostics go to stderr. Every error and warning carries a stable
code and its fix.

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every selected unit is current. |
| 1 | A configuration, resolution, or operational error, such as `NYL-STATE-MISSING`, `NYL-SOURCE-UNREACHABLE`, `NYL-UNIT-UNKNOWN`, or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | Not everything was reconciled and nothing failed: a unit is blocked, or another run holds the lease (`NYL-LEASE-HELD`). |
| 3 | At least one execution failed. |
| 4 | At least one execution is uncertain, the run lost its lease (`NYL-LEASE-LOST`), or it was cancelled (`NYL-RUN-CANCELLED`). |

When several apply, the most severe wins, in the order 4, 3, 1, 2, 0. With
`--unit`, only the named units count.

## Codes

Besides the codes above, a reconcile reports:

| Code | Meaning and fix |
| --- | --- |
| `NYL-UNIT-UNCERTAIN` | A unit may have had effects without a recorded result. Check what it did; a converging unit runs again in the next reconcile, others need [`nyl recover`](/nyl/reference/cli/recover/). |
| `NYL-RUN-SUPERSEDED` | A dead run checkpointed a unit with an execution key the new resolution no longer has. Its effects are real, so the unit becomes `uncertain` when the result recorded effects. |
| `NYL-RUN-LATE-CHECKPOINT` | A run whose lease was broken checkpointed a unit after it was marked `uncertain`. The checkpoint stays as evidence; adopt it with `nyl recover --accept <run>` or run the unit again with `--retry`. |
| `NYL-RUN-CANCELLED` | The run was cancelled by a signal. Run `nyl reconcile` again to take over its checkpoints. |
| `NYL-LEASE-LOST` | Another invocation took over or deleted the lease. The run pushed nothing; the next run imports its checkpoints. |
| `NYL-STATE-LOCAL-TIP` | A remote state ref's tip is a `Nyl-Local` commit pushed by hand. Repair it with [`nyl state restore`](/nyl/reference/cli/state/#nyl-state-restore). |
| `NYL-LOCAL-RUN-UNRECORDED` | A `--local` run executed a unit that remote state does not know about. Record it with a remote `nyl reconcile -e <env> --unit <unit>` once the change is committed. |
| `NYL-LOCAL-RUN-UNSUPPORTED` | `--local` needs remote state with state refs that are branches. |
| `NYL-LOCAL-RUN-LEFTOVERS` | Remote state holds runs that died and were not taken over. Run `nyl reconcile -e <env>` without `--local` first. |

[`nyl plan`](/nyl/reference/cli/plan/) and [`nyl status`](/nyl/reference/cli/status/)
share `-e`, `-o`, and `[PATH]`, and use the same exit categories.
