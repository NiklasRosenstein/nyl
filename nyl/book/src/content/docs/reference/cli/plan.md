---
title: 'plan'
---

Shows what [`nyl reconcile`](/nyl/reference/cli/reconcile/) would do, without
taking the lease or writing anything. A pull request pipeline runs it against
the pull request's head, so reviewers see what a merge would change.

```bash
nyl plan -e dev
nyl plan -e prod --source origin/main
nyl plan -e dev --unit migrate
nyl plan -e dev --fail-on-leaving -o json
nyl plan -e dev --teardown --unit cache
nyl plan -e dev --teardown --all
```

`plan` resolves the environment like a run (see [Runs](/nyl/orchestration/runs/)),
asks each ready unit's driver what executing it would change, and reports:

- first, the units that would leave the ownership set because they were
  removed from source, deselected by the Environment's `unitSelector`, or
  disabled, with the warning `NYL-PLAN-UNITS-LEAVING`. They are grouped by what
  their deletion will do: torn down (`teardown`, which needs
  `--allow-teardown`), dropped from state with their resources kept
  (`dropped`), or kept as a tombstone under `deletionPolicy: Retain`
  (`retained`). A deselected unit names the selector labels it no longer
  matches, so a selector typo reads as one;
- each leaving or deleting unit with its state (`pending-teardown`,
  `tearing-down`, `retained`, or `held`), the driver's destroy plan
  (`changes: destroy`) of a teardown it would start or that waits only for
  `--allow-teardown`, and why it waits, such as `NYL-TEARDOWN-PENDING`,
  `NYL-TEARDOWN-HAS-DEPENDENTS` naming the tombstones that cite it, or a
  publication's unmet teardown readiness. Retained tombstones are listed as
  `retained`. See [Deletion and holds](/nyl/orchestration/deletion/);
- each unit's state: `ready` for a unit that would execute, with the driver's
  change summary and digest, `drifted` for a current unit whose drift the
  run would repair, `current`, `held`, or `blocked` with the references it
  waits for. A unit whose last execution failed with a retryable failure, or
  is uncertain under a recovery policy that runs it again, is reported
  `ready`.

```console
$ nyl plan -e dev
Units leaving the ownership set of dev (2):
  teardown, needs --allow-teardown:   cache (removed)
  dropped from state, resources kept: (none)
  retained as tombstones:             worker (removed)

UNIT    KIND     STATE             RECEIPT  CHANGES  DETAIL
cache   Command  pending-teardown  -        destroy  run sh
worker  Command  pending-teardown  -        -        would be retained: its resources stay, and its state becomes a tombstone

  source:   a16f93e6a1b69c767af8a3f37c84e6f102c20858

warning NYL-PLAN-UNITS-LEAVING
  2 unit(s) leave the ownership set of dev: cache (removed), worker (removed)
  fix: restore the units in source or in the unitSelector's labels if they should stay; pass --fail-on-leaving to plan to make removals fail a pipeline

warning NYL-TEARDOWN-PENDING (cache)
  unit cache of dev left the ownership set; its teardown waits for --allow-teardown
  fix: pass --allow-teardown to nyl reconcile to tear it down, restore it in source to keep it, or let it go without a teardown with `nyl state forget -e dev --unit cache`
```

`plan --teardown` previews what [`nyl teardown`](/nyl/reference/cli/teardown/)
would do for the `--unit` units, or with `--all` for every unit: the destroy
plans of the teardowns it would start, the units it would release, and why
the others wait. Like `nyl teardown`, it reads only the documents in state and
renders nothing, so it works when source no longer declares the units.

`plan` reads state with read access only and needs no commit identity.

Select several environments with repeated `-e`, with `-l stage=production`, or
with `-g <group>`. They run in dependency order with one report section per
environment. See [Multi-environment runs](/nyl/orchestration/runs/#multi-environment-runs)
for qualified unit options and independent state transactions.

`--unit` filters the report while resolution still covers every desired unit.
A changed producer outside that filter still blocks a selected consumer in
another environment until its new receipt exists.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | Select named environments; repeatable. Use exactly one of `-e`, `-l`, or `-g`. |
| `-l`, `--labels <key=value>` | Select declared environments by literal labels; comma-separated pairs use AND. |
| `-g`, `--group <name>` | Select the members of an EnvironmentGroup. |
| `--unit <name>`, `--units <a>,<b>` | Report only these units. Resolution still covers the whole environment, and the leaving section is never filtered. |
| `--source <rev>` | Plan at this revision or commit instead of the Environment's source. With `--teardown`, preview the teardowns with that revision's files instead of each unit's recorded `sourceRevision`. |
| `--teardown` | Preview `nyl teardown` of the `--unit` units, or of every unit with `--all`, from the documents in state. |
| `--all` | With `--teardown`, preview tearing down every unit. |
| `--fail-on-leaving` | Exit 1 when any unit would leave the ownership set, for pipelines that want removals to need an explicit override. |
| `-o`, `--output <table\|json>` | A table, or the typed `PlanReport`, which lists leaving units under `leaving`, each with its `reason`, `consequence` (`teardown`, `dropped`, or `retained`), and, when deselected, the unmatched selector `labels`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | The plan is complete, whatever changes it shows. |
| 1 | An error, or `--fail-on-leaving` with a unit leaving. |
| 2 | Incomplete: a unit is blocked, so its change cannot be planned. A pending teardown does not raise the exit. |
| 3 | A driver's planning failed; the unit's report carries `planError`. |
