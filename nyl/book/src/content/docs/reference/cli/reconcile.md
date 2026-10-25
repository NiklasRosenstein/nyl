---
title: 'reconcile'
---

Executes an environment's ready units in dependency order and records the
results in its state. See [Runs](/nyl/orchestration/runs/) for how a run
chooses its source commit, resolves references, and decides what executes.

:::note
Orchestration is being built in milestone M3. `reconcile` runs one
environment's [Command](/nyl/orchestration/resource-guides/command/) units;
recovery, teardown, holds, approvals, and runs over several environments
follow.
:::

```bash
nyl reconcile -e dev
nyl reconcile -e dev --unit migrate
nyl reconcile -e dev --units database,migrate --concurrency 2
nyl reconcile -e prod --source 3e7b9c1 --author "Release Bot <release@example.com>"
nyl reconcile -e dev -o json
```

The environment's state must exist: create it once with
[`nyl state init`](/nyl/reference/cli/state/). The run takes the environment's
lease, executes waves of ready units until nothing is ready, writes one
transition commit per state ref it changes, and releases the lease. A run
that changes nothing writes no commit.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment to reconcile. Required. |
| `--unit <name>`, `--units <a>,<b>` | Execute only these units. Resolution still covers the whole environment, and a named unit whose dependency has no current receipt stays blocked. An unknown name exits 1 with `NYL-UNIT-UNKNOWN`. |
| `--concurrency <n>` | Units a wave executes at once. Default 4. |
| `--source <rev>` | Render units at this revision or commit instead of the Environment's source, for this run only. It must pass the same [reachability checks](/nyl/orchestration/runs/#source-commit), and the transition commit marks it as an override. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease. |
| `--local` | Run next to remote state without publishing it: continue from this clone's [local state branches](/nyl/orchestration/environments-and-state/#local-runs) and commit there, marked `Nyl-Local: true`. Warns `NYL-LOCAL-RUN-UNRECORDED` for every unit it executes. Exits 1 with `NYL-LOCAL-RUN-UNSUPPORTED` for state without a remote or with full `refs/…` state refs, and with `NYL-LOCAL-RUN-LEFTOVERS` while remote state has dead runs to take over. |
| `--local-lease` | With `--local`, take this clone's own lease instead of the remote one, accepting that CI may run at the same time. |
| `--approve <unit>` | With `--local`, execute this unit although its recovery policy is not `converge`, recorded with the `local` approval identity. Repeatable or comma-separated. Other such units wait as `awaiting-approval`. |
| `--author "Name <email>"` | The author of the state commits. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table of units, or the typed `ReconcileReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

The table lists each unit with its result: `executed`, `current`, `failed`,
`uncertain`, `blocked`, or `awaiting-approval`. `-o json` prints the same
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
| 4 | At least one execution is uncertain, or the run lost its lease (`NYL-LEASE-LOST`). |

When several apply, the most severe wins, in the order 4, 3, 1, 2, 0. With
`--unit`, only the named units count.

[`nyl plan`](/nyl/reference/cli/plan/) and [`nyl status`](/nyl/reference/cli/status/)
share `-e`, `-o`, and `[PATH]`, and use the same exit categories.
