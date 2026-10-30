---
title: 'reconcile'
---

Executes an environment's ready units in dependency order and records the
results in its state. See [Runs](/nyl/orchestration/runs/) for how a run
chooses its source commit, resolves references, and decides what executes.

:::note
`reconcile` runs selected environments' Command, OciImage, Terraform, and
OpenTofu units, with recorded approvals, recovery, drift repair, and lifecycle
handling. Kubernetes publication and promotion drivers follow in M5 and M6.
:::

```bash
nyl reconcile -e dev
nyl reconcile -e dev --unit migrate
nyl reconcile -e dev --units database,migrate --concurrency 2
nyl reconcile -e prod --source 3e7b9c1 --author "Release Bot <release@example.com>"
nyl reconcile -e dev -o json
nyl reconcile -e dev --wait-lease 30m
nyl reconcile -e dev --local --approve migrate
nyl reconcile -e dev --allow-teardown
nyl reconcile -e dev --leaving --allow-teardown
nyl reconcile -e dev --unit config --reapply --reason "rotate credentials"
nyl reconcile -e dev --fail-fast
```

The environment's state must exist: create it once with
[`nyl state init`](/nyl/reference/cli/state/). The run takes the environment's
lease, executes waves of ready units until nothing is ready, writes one
transition commit per state ref it changes, and releases the lease. A run
that changes nothing writes no commit.

## Deletion

After the execution waves, a run carries out the lifecycle work of units
that left the ownership set and of teardowns an operator requested with
[`nyl teardown`](/nyl/reference/cli/teardown/), consumers first, as
[Deletion and holds](/nyl/orchestration/deletion/) describes. A unit with
`deletionPolicy: Teardown` that left source waits for `--allow-teardown`:

```console
$ nyl reconcile -e dev
Units leaving the ownership set of dev (1):
  teardown, needs --allow-teardown:   scratch (removed)
  dropped from state, resources kept: (none)
  retained as tombstones:             (none)

UNIT     KIND     RESULT            DETAIL
cache    Command  current
config   Command  current
extra    Command  executed
scratch  Command  pending-teardown
seed     Command  current
worker   Command  current
run: 01a17ac1-4c6f-7d70-8d17-67644811eab7
source: ea88fa3d6330a476c69ac698edbedab8cc5b7be7
desired: 81117d9f1baa8fa2c8db45139c3cf486b8d3879e
observed: 7b18f7fa52729b27399b285f1ed76dbdfdf29fdb
warning NYL-PLAN-UNITS-LEAVING: 1 unit(s) leave the ownership set of dev: scratch (removed)
  fix: restore the units in source or in the unitSelector's labels if they should stay; pass --fail-on-leaving to plan to make removals fail a pipeline
warning NYL-TEARDOWN-PENDING (scratch): unit scratch of dev left the ownership set; its teardown waits for --allow-teardown
  fix: pass --allow-teardown to nyl reconcile to tear it down, restore it in source to keep it, or let it go without a teardown with `nyl state forget -e dev --unit scratch`
$ nyl reconcile -e dev --allow-teardown
removed scratch
Units leaving the ownership set of dev (1):
  teardown, needs --allow-teardown:   scratch (removed)
  dropped from state, resources kept: (none)
  retained as tombstones:             (none)

UNIT     KIND     RESULT   DETAIL
cache    Command  current
config   Command  current
extra    Command  current
scratch  Command  removed
seed     Command  current
worker   Command  current
run: 01a17ac1-4e91-77be-904d-da53c4fd269f
source: ea88fa3d6330a476c69ac698edbedab8cc5b7be7
desired: 182633b7ab899fc7c2f0c836644fa10bcc6e5409
observed: 02dac2dc31d29972006a5583de6f4e32365e0782
warning NYL-PLAN-UNITS-LEAVING: 1 unit(s) leave the ownership set of dev: scratch (removed)
  fix: restore the units in source or in the unitSelector's labels if they should stay; pass --fail-on-leaving to plan to make removals fail a pipeline
```

`--allow-teardown` gates only the start of a teardown caused by a change to
source; a teardown that started, and one requested with `nyl teardown`,
continue without it unless the unit is held. A held unit changes in no way:
it is reported `held` with the source commit whose changes wait.

## Drift

A unit whose latest [drift observation](/nyl/reference/cli/verify/) is
`drifted` runs again when its recovery policy is `converge` and the
Environment's `driftRepair` is `converge`, the default, and the transition
commit lists it under `driftRepaired`. A drifted unit that is not safe to
re-run is `awaiting-approval` while the rest of the run continues. Under `driftRepair: manual`,
drifted units stay `drifted` until `--reapply` names them. See
[Repairing drift](/nyl/orchestration/recovery/#repairing-drift).

```console
$ nyl reconcile -e dev
UNIT    KIND     RESULT             DETAIL
cache   Command  current
config  Command  executed
extra   Command  current
seed    Command  awaiting-approval
worker  Command  current
run: 01a17ac1-5904-7e98-993d-d452167aa129
source: ea88fa3d6330a476c69ac698edbedab8cc5b7be7
observed: e9030d4534b190e1a82161dc15d0c0f29e6c90b2
```

## Stopping at the first failure

A run reconciles everything it can: a unit that fails, becomes uncertain,
waits for an approval, or is blocked does not stop the units that do not
depend on it. `--fail-fast` starts no further unit after the first failure,
uncertain outcome, or approval wait; units already running finish, and the
run still checkpoints them and writes its transition commit:

```console
$ nyl reconcile -e dev --fail-fast --concurrency 1
connection refused
UNIT    KIND     RESULT   DETAIL
broken  Command  failed   command exited with code 1
cache   Command  current
config  Command  current
extra   Command  current
report  Command  blocked  --fail-fast stopped the run after broken
seed    Command  current
worker  Command  current
run: 01a17ac1-5cb2-7c73-8a5b-46bb04b9cf5c
source: 1af28df1e4f0db7bdab23402106123ce1592a47c
desired: c30d18d44b74895ae0dd147201ad18762d36d26d
observed: 0b2fd370c62ab5c01bc9199954bdcfe9b17eca44
warning NYL-RUN-STOPPED: --fail-fast stopped the run of dev after unit broken: it started no further unit
  fix: fix broken, then run again; without --fail-fast a run continues with every unit that does not depend on it
```

## Takeover

When the lease has expired, or was broken with
[`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break), the run
replaces it and imports the run refs earlier runs left behind, as
[Recovery](/nyl/orchestration/recovery/#taking-over) describes: a
checkpoint with the unit's current execution key becomes its receipt
(`imported`), a unit a dead run was executing without a checkpoint becomes
`uncertain` (`recovered`), and a converging uncertain unit executes again in
the same run. The transition commit lists both under `imported` and
`recovered`, and a broken lease once under `leaseBroken`. While a live run
holds the lease or its run ref still carries a deadline, the run exits 2 with
`NYL-LEASE-HELD`, unless `--wait-lease` waits for it.

## Cancellation

SIGINT or SIGTERM cancels the run: it starts no further unit, forwards the
signal to running tools, and kills a tool still running 30 seconds later. It
checkpoints the units that finish, marks the ones still running
`uncertain`, keeps its run ref for the next run to take over, releases the
lease, writes no transition commit, and exits 4 with `NYL-RUN-CANCELLED`.
A second signal ends the process at once, with exit code 128 plus the signal
number, as a forced kill would; the next run takes over as after a crash. A
signal while `--wait-lease` waits for another run's lease also exits 4 with
`NYL-RUN-CANCELLED`, having written nothing.

Select several environments with repeated `-e`, with `-l stage=production`, or
with `-g <group>`. They run in dependency order with one report section per
environment. See [Multi-environment runs](/nyl/orchestration/runs/#multi-environment-runs)
for qualified unit options and independent state transactions.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | Select named environments; repeatable. Use exactly one of `-e`, `-l`, or `-g`. |
| `-l`, `--labels <key=value>` | Select declared environments by literal labels; comma-separated pairs use AND. |
| `-g`, `--group <name>` | Select the members of an EnvironmentGroup. |
| `--unit <name>`, `--units <a>,<b>` | Execute only these units, and do no lifecycle work. Resolution still covers the whole environment, and a named unit whose dependency has no current receipt stays blocked. An unknown name exits 1 with `NYL-UNIT-UNKNOWN`. |
| `--allow-teardown` | Start the teardown of units that left the ownership set, or changed their kind, under `deletionPolicy: Teardown`. Without it they stay `pending-teardown`, exit 2. |
| `--allow-dependents` | Tear down a unit although retained tombstones cite it; the transition commit records the override under `allowDependents`. |
| `--allow-incomplete` | Start a publication's teardown although its target's settings do not let it complete, and end its teardown wait without a claim of removal; the commit records what may remain under `incomplete`. |
| `--leaving` | Do only the lifecycle work of units outside the ownership set. Conflicts with `--unit`. |
| `--reapply` | Execute the units named by `--unit` although their receipts are current, such as to repair drift under `driftRepair: manual`. Requires `--unit` and `--reason`; recorded under `reapplied`. A named unit that is not safe to re-run awaits approval, or runs with a matching `--approve`. |
| `--reason <text>` | Why, for `--reapply`; recorded with the `Nyl-Reason` trailer. |
| `--fail-fast` | Start no further unit after the first failure, uncertain outcome, or approval wait. Units not started report `blocked`, and the run warns with `NYL-RUN-STOPPED`. |
| `--concurrency <n>` | Units a wave executes at once. Default 4. |
| `--source <rev>` | Render units at this revision or commit instead of the Environment's source, for this run only. It must pass the same [reachability checks](/nyl/orchestration/runs/#source-commit), and the transition commit marks it as an override. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease, instead of exiting 2 with `NYL-LEASE-HELD`. |
| `--local` | Operate on the configured local state repository without fetching, pushing, or taking a remote lease. |
| `--approve <unit>[=<digest>]` | Approve a manual unit, a drift repair that is not safe to repeat, or a non-converging unit in a local run. Repeatable or comma-separated. A desired-bound approval covers its initial execution key; a plan-bound digest covers the reviewed changes. Excluded units are an error. See [Approvals](/nyl/orchestration/runs/#approvals). |
| `--approve-all` | Approve every candidate without a reviewed digest; policies with `requireDigest: true` still wait. Explicit digests override this for named units. |
| `--approved-by <name>`, `--approval-source <source>` | Assert approver identities and their source. Names are repeatable or comma-separated; recorded as `asserted`. Without these flags Nyl uses a configured lookup, CI context, or the Git user. |
| `--author "Name <email>"` | The author of the state commits. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table of units, or the typed `ReconcileReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

The table lists each unit with its result: `executed`, `current`,
`imported`, `recovered`, `removed`, `retained`, `released`, `failed`,
`uncertain`, `held`, `drifted`, `pending-teardown`, `tearing-down`,
`blocked`, or `awaiting-approval`, as
[Runs](/nyl/orchestration/runs/#transition-commits) defines them. A run with
units leaving the ownership set prints the leaving section first, as
[`nyl plan`](/nyl/reference/cli/plan/) does. `-o json` prints the same
report as JSON on stdout, with `result` per unit as in the transition commit
summary; diagnostics go to stderr. Every error and warning carries a stable
code and its fix.

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every selected unit is current, or its deletion completed (`removed`, `retained`, `released`). |
| 1 | A configuration, resolution, or operational error, such as `NYL-STATE-MISSING`, `NYL-SOURCE-UNREACHABLE`, `NYL-UNIT-UNKNOWN`, or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | Not everything was reconciled and nothing failed: a unit is blocked, held, drifted, awaiting approval, `pending-teardown`, or `tearing-down`; another run holds the lease (`NYL-LEASE-HELD`). |
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
| `NYL-RUN-UNIMPORTED` | Reported by `nyl status`: a run ended without its transition commit, cancelled or after losing its lease, and the next reconcile imports its checkpoints. Units it left uncertain show as `uncertain` until then. |
| `NYL-RUN-CANCELLED` | The run was cancelled by a signal. Run `nyl reconcile` again to take over its checkpoints. |
| `NYL-LEASE-LOST` | Another invocation took over or deleted the lease. The run pushed nothing; the next run imports its checkpoints. |
| `NYL-RUN-STOPPED` | `--fail-fast` stopped the run after a unit failed, became uncertain, or waited for an approval. Fix that unit and run again. |
| `NYL-UNIT-HELD` | A held unit keeps source changes or a teardown from applying. Lift the hold with `nyl resume`. |
| `NYL-PLAN-UNITS-LEAVING` | Units leave the ownership set. Restore them in source or the `unitSelector` if they should stay. |
| `NYL-TEARDOWN-PENDING` | A teardown waits for `--allow-teardown`, its deleting dependents, or an approval. |
| `NYL-TEARDOWN-HAS-DEPENDENTS` | Retained tombstones cite the unit. Pass `--allow-dependents`, or forget the tombstones. |
| `NYL-TEARDOWN-WAITING` | A publication it depends on waits under its `teardownWait`. Run again later, or confirm with `nyl teardown --confirm-removed`. |
| `NYL-TEARDOWN-INCOMPLETE` | A teardown cannot complete, or completed without a claim of removal. Meet the requirement or pass `--allow-incomplete`, and check what remains. |
| `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`, `NYL-TEARDOWN-APPLICATIONS-ORPHANED`, `NYL-TEARDOWN-NAMESPACE-CONFIRM` | A publication's target does not let its teardown complete; each names the setting that fixes it. |
| `NYL-TEARDOWN-NAMESPACES-REMAIN`, `NYL-TEARDOWN-CATALOG-REMAINS` | A publication's teardown leaves these objects behind. |
| `NYL-UNIT-RELEASED` | A unit's resources were left in place. Remove them by hand when nothing needs them. |
| `NYL-UNIT-KIND-CHANGE` | A unit changed its kind; the old incarnation is deleting first. |
| `NYL-STATE-DELETED` | The environment's state was deleted with `nyl state delete`. Remove the Environment, or start over with `nyl state init --fresh`. |

[`nyl plan`](/nyl/reference/cli/plan/) and [`nyl status`](/nyl/reference/cli/status/)
share `-e`, `-o`, and `[PATH]`, and use the same exit categories.
