---
title: 'reconcile'
---

Executes an environment's ready units in dependency order and records the
results in its state. See [Runs](/nyl/orchestration/runs/) for how a run
chooses its source commit, resolves references, and decides what executes.

:::note
`reconcile` runs selected environments' Command, OciImage, Terraform,
OpenTofu, and [KubernetesPublication](/nyl/orchestration/resource-guides/kubernetes-publication/)
units, with recorded approvals, recovery, drift repair, and lifecycle
handling. Promotion drivers follow in M6.
:::

```bash
nyl reconcile -e dev
nyl reconcile -e dev --unit migrate
nyl reconcile -e dev --units database,migrate --concurrency 2
nyl reconcile -e prod --source 3e7b9c1 --author "Release Bot <release@example.com>"
nyl reconcile -e dev -o json
nyl reconcile -e dev --wait-lease 30m
nyl reconcile -e dev --local --approve migrate
nyl reconcile -e dev --worktree
nyl reconcile -e dev --allow-teardown
nyl reconcile -e dev --leaving --allow-teardown
nyl reconcile -e dev --unit config --reapply --reason "rotate credentials"
nyl reconcile -e dev --fail-fast
nyl reconcile -e pr-123 --template preview --param pr=123
nyl reconcile -e pr-123 --renew
nyl reconcile --template preview
```

The environment's state must exist: create it once with
[`nyl state init`](/nyl/reference/cli/state/). The run takes the environment's
lease, executes waves of ready units until nothing is ready, writes one
transition commit per state ref it changes, and releases the lease. A run
that changes nothing writes no commit.

## Preview instances

`--template <name>` with one `-e <instance>` and `--param <name>=<value>`
creates the instance of an
[EnvironmentTemplate](/nyl/orchestration/resource-guides/environment-template/)
or updates its parameters, as [`nyl state init --template`](/nyl/reference/cli/state/#nyl-state-init)
does, and then reconciles it from this checkout: the run records the checkout's
commit as the instance's source. Without `--template`, the environment is a
declared Environment or an existing instance found by its record, and an
instance renders at the source commit its record names, whichever checkout the
command runs from; the report says so, and the environment record keeps
`by: recorded`.

## Preview lifecycle

Instances expire, and a reconcile is how they stay alive or are removed. The
full rules are in
[expiry, limits, and the fleet](/nyl/orchestration/resource-guides/environment-template/#expiry-limits-and-the-fleet);
the options are:

| Command | An instance that is live | An instance that is expired |
| --- | --- | --- |
| `reconcile -e <instance>` | extends `expiresAt` to now plus `ttl`, then reconciles | removes it |
| `reconcile -e <instance> --no-extend` | reconciles without extending | removes it |
| `reconcile -e <instance> --renew` | extends, then reconciles | extends, then reconciles |
| `reconcile -e <instance> --template <name> --param …` | creates or updates it, extends, then reconciles | the same: updating an active pull request never removes its preview |
| `reconcile --template <name>` | reconciles at its own recorded commit, without extending | removes it |

- **Removal** runs `teardown --all` and then `state delete`, the way [`nyl state
  delete --teardown`](/nyl/reference/cli/state/#nyl-state-delete) does, and
  reports the instance as `removed` under `instance.action` (`-o json`). The
  state stays while any unit has an incarnation, so a teardown that waits,
  fails, or is uncertain leaves the instance in place (`removal-incomplete`),
  and the exit is that of the teardown: 2 for a wait, 3 for a failure, 4 for
  an uncertain outcome. The teardown starts only when the template sets
  `allowTeardown` or the run passes `--allow-teardown`; otherwise the units
  are recorded as pending teardown and the run exits 2
  (`NYL-INSTANCE-REMOVAL-PENDING`, `removal-pending`). `--allow-dependents`,
  `--allow-incomplete`, `--approve`, `--source`, and `--confirm-removed`
  (through [`nyl teardown`](/nyl/reference/cli/teardown/)) apply as they do to a
  teardown. A removal decided on one `expiresAt` repeats the check under the
  lease, so an instance another run renewed meanwhile is kept
  (`NYL-INSTANCE-RENEWED`, exit 2).
- **`--renew`** keeps an expired instance. **`--no-extend`** reconciles an
  instance as maintenance. They conflict, and `--no-extend` conflicts with
  `--template`. Both are rejected for a declared Environment, which has no
  expiry, and `--renew`, `--param`, `--make-room`, `--source`, and
  `--worktree` for the fleet (`NYL-INSTANCE-OPTION-INVALID`).
- **`--make-room`**, with `--template` and `-e`, removes expired instances of
  a template at `maxInstances` first, oldest expiry first and only as many as
  the new instance needs, then creates the instance; without it the run exits 2
  (`NYL-INSTANCE-LIMIT`) and names the expired instances that could go.
- **`reconcile --template <name>` without `-e`** is the scheduled fleet job. It
  finds the template's instances by listing its state location, reconciles each
  live one at its own recorded source commit, removes the expired ones, and
  never extends an expiry. An instance whose lease another run holds is
  skipped and reported as `busy` (`instance.action`), which does not fail the
  run: the exit category reflects only the instances it processed. With no
  instance it exits 0. The template's state location must be listable
  (`NYL-INSTANCE-NOT-LISTABLE`).

`status`, `plan`, and `nyl get environments` show an expired instance until a
reconcile removes it, and warn with `NYL-INSTANCE-EXPIRED`. Every instance
report carries `instance.template`, `instance.expiresAt`, and
`instance.expired`.

## Observed values

A run reads the [observed values](/nyl/orchestration/runs/#observed-values) of
every unit that has a current receipt and declares `observations` once, before it
resolves the unit's consumers, and after it executed such a unit waits up to
the unit's `observationWait` for values that are still pending, polling about
every 15 seconds and only while no other unit is ready. A consumer of a pending
value is blocked (`NYL-OBSERVATION-PENDING`, exit 2); a changed value makes its
consumers execute again in the same run, without the producer executing. A read
that fails warns (`NYL-OBSERVATION-FAILED`) and keeps the last recorded value.
The transition commit's summary lists the observations whose value changed under
`observationsChanged`. The run needs the read credentials the unit admits; a job
without them can leave the reading to [`nyl observe`](/nyl/reference/cli/observe/)
elsewhere.

## Deletion

`plan` and `reconcile` warn about each unmet teardown requirement, and each
object a teardown would leave behind, of every publication that would be torn
down (the `NYL-TEARDOWN-*` codes below), so a target that cannot tear down
cleanly is known before a teardown runs.

After the execution waves, a run carries out the lifecycle work of units
that left the ownership set and of teardowns an operator requested with
[`nyl teardown`](/nyl/reference/cli/teardown/), consumers first, as
[Deletion and holds](/nyl/orchestration/deletion/) describes. A unit with
`deletionPolicy: Teardown` that left source waits for `--allow-teardown`:

```bash
nyl reconcile -e dev
nyl reconcile -e dev --allow-teardown
```

`--allow-teardown` gates only the start of a teardown caused by a change to
source; a teardown that started, and one requested with `nyl teardown`,
continue without it unless the unit is held. A held unit changes in no way:
it is reported `held` with the source commit whose changes wait.

## Drift

A unit whose latest [drift record](/nyl/reference/cli/detect-drift/) is
`drifted` runs again when its recovery policy is `converge` and the
Environment's `driftRepair` is `converge`, the default, and the transition
commit lists it under `driftRepaired`. A drifted unit that is not safe to
re-run is `awaiting-approval` while the rest of the run continues. Under `driftRepair: manual`,
drifted units stay `drifted` until `--reapply` names them. See
[Repairing drift](/nyl/orchestration/recovery/#repairing-drift).


## Stopping at the first failure

A run reconciles everything it can: a unit that fails, becomes uncertain,
waits for an approval, or is blocked does not stop the units that do not
depend on it. `--fail-fast` starts no further unit after the first failure,
uncertain outcome, or approval wait; units already running finish, and the
run still checkpoints them and writes its transition commit:

```console
$ nyl reconcile -e dev --fail-fast --concurrency 1
[dev/broken] starting Command (reconcile): no receipt
  [dev/broken] connection refused
[dev/broken] failed in 0.0s: command exited with code 1
UNIT    KIND     RESULT   DETAIL
broken  Command  failed   command exited with code 1
report  Command  blocked  /dependsOn/0 waits for broken (no-current-receipt)

  run:      01a18581-e81a-76b7-b92e-dde094292715
  source:   31d0b75f7f54f720a76847c6dacf768371f0ca63
  desired:  4b5fd9df85d0db0c82181710bc9b457a582a8e22
  observed: 63b784cd15c55e8337adf1379c2bc6e09e69e611

warning NYL-RUN-STOPPED
  --fail-fast stopped the run of dev after unit broken: it started no further unit
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
| `--template <name>`, `--param <name>=<value>` | Create or update `-e <instance>` of this EnvironmentTemplate before reconciling it. `--param` is repeatable and needs `--template`; one `-e` only. |
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
| `--worktree` | Snapshot Git-visible working files, including uncommitted and untracked files, and reconcile exactly one named environment from them. Conflicts with `--source`, `-l`, and `-g`. State publication follows the configured repository or `--local`. See [worktree inputs](/nyl/orchestration/runs/#worktree-inputs). |
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
summary. Each environment's `unitOrder` lists the table order while `units`
remains a map keyed by name. The table lists dispatched units first in dispatch
order, then units that did not run in dependency order; teardown uses consumers
first. Names break ties within a parallel wave.

Unit start and end messages stream to stderr with the environment/unit address,
result, elapsed time, and a short failure reason. Public tool output is indented
and carries that address on every line; captured outputs retain their original
bytes. [Execution output controls](/nyl/orchestration/runs/#following-execution)
can hide tool output (`--quiet-unit-output`), append masked unit logs to files
(`--unit-log-dir DIR`), or do both.
`-o json` keeps stdout exclusively for the report. Every error and warning carries
a stable code and its fix.

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
| `NYL-SOURCE-WORKTREE` | The run uses an immutable working-file snapshot retained in state history. Commit changes and run without `--worktree` for committed-source evidence. |
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
| `NYL-KUBERNETES-OWNER-FENCED` | Another existing unit incarnation owns the publication prefix, so nothing was published (a retryable failure). Tear that unit down or release it with `nyl state forget`; see [Ownership](/nyl/orchestration/resource-guides/kubernetes-publication/#ownership). |
| `NYL-KUBERNETES-OWNER-UNKNOWN` | The prefix's owner is in an environment that was renamed, removed, or cannot be read, so the fence holds. Declare the environment again and tear its unit down, or release the prefix as the same page describes. |
| `NYL-KUBERNETES-TARGET-CHANGED` | A publication's target name or destination changed since it last published, and nothing was published. Restore it, or run `nyl teardown --unit <u>` and reconcile again. |
| `NYL-KUBERNETES-OBSERVATION-UNOWNED` | A publication observes a resource its target does not render. Observe a resource one of the target's Releases renders. |
| `NYL-OBSERVATIONS-UNSUPPORTED` | A unit declares `observations`, but its kind cannot read observed values for its spec, such as a Command, or a publication in `mode: publish`. |
| `NYL-OBSERVATION-PENDING` | A consumer waits for an observed value that its producer's current receipt has none of yet. Run `nyl observe` or `nyl reconcile` once the system holds it, or give the producer an `observationWait`. |
| `NYL-OBSERVATION-FAILED` | An observed value could not be read; the last recorded value stays usable. Fix the cause, such as missing credentials. |
| `NYL-OBSERVATION-CHANGED` | Reported by `nyl status` and `nyl observe`: an observed value changed or arrived, so its consumers are ready for the next reconcile. |
| `NYL-UNIT-RELEASED` | A unit's resources were left in place. Remove them by hand when nothing needs them. |
| `NYL-UNIT-KIND-CHANGE` | A unit changed its kind; the old incarnation is deleting first. |
| `NYL-STATE-DELETED` | The environment's state was deleted with `nyl state delete`. Remove the Environment, or start over with `nyl state init --fresh`. |

[`nyl plan`](/nyl/reference/cli/plan/) and [`nyl status`](/nyl/reference/cli/status/)
share `-e`, `-o`, and `[PATH]`, and use the same exit categories.
