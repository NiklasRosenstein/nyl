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
- each unit's state: `ready` for a unit that would execute, with the driver's
  change summary and digest, `current`, or `blocked` with the references it
  waits for. A unit whose last execution failed with a retryable failure, or
  is uncertain under a recovery policy that runs it again, is reported
  `ready`.

```text
Units leaving the ownership set of dev (2):
  teardown, needs --allow-teardown:   (none)
  dropped from state, resources kept: seed (deselected, no longer matches dve=true)
  retained as tombstones:             cache (removed)
```

`plan` reads state with read access only and needs no commit identity.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment to plan. Required. |
| `--unit <name>`, `--units <a>,<b>` | Report only these units. Resolution still covers the whole environment, and the leaving section is never filtered. |
| `--source <rev>` | Plan at this revision or commit instead of the Environment's source. |
| `--fail-on-leaving` | Exit 1 when any unit would leave the ownership set, for pipelines that want removals to need an explicit override. |
| `-o`, `--output <table\|json>` | A table, or the typed `PlanReport`, which lists leaving units under `leaving`, each with its `reason`, `consequence` (`teardown`, `dropped`, or `retained`), and, when deselected, the unmatched selector `labels`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | The plan is complete, whatever changes it shows. |
| 1 | An error, or `--fail-on-leaving` with a unit leaving. |
| 2 | Incomplete: a unit is blocked, so its change cannot be planned. |
| 3 | A driver's planning failed; the unit's report carries `planError`. |
