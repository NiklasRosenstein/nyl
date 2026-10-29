---
title: 'status'
---

Reports each unit's state from one snapshot of an environment's desired and
observed state. CI can use its exit code to check whether everything is
reconciled.

```bash
nyl status -e dev
nyl status -e dev -o json
```

Each unit has one state: `blocked`, `ready`, `awaiting-approval`, `running`,
`uncertain`, `failed`, `current`, `drifted`, `held`, `pending-teardown`,
`tearing-down`, or `retained`. Whether its latest receipt is still current is
shown separately, as `current`, `stale`, or `none`. For the documents behind a
state, use [`nyl get unit <name> -e <env>`](/nyl/reference/cli/project-resources/#inspect-environment-state).

- **Lifecycle states.** `held`, `pending-teardown`, `tearing-down`, and
  `retained` follow the unit's `lifecycle` block. A held or deleting unit
  with a condition shows it next to the state, such as `held (uncertain)` or
  `tearing-down (failed)`, and the condition decides the exit code: a hold
  never hides that a unit still needs recovery. Retained tombstones are
  listed; their resources stay by policy, so they do not change the exit
  code.
- **Held units.** A held unit's detail names the source commit whose changes
  the hold keeps from applying ("pending changes from <commit>") and, after a
  `nyl teardown` of the held unit, "teardown pending, carried out after
  resume". A held marker, written by `nyl hold` for a unit the environment
  never reconciled, has no kind (`-`) and no receipt. Each held unit warns
  with `NYL-UNIT-HELD`, naming [`nyl resume`](/nyl/reference/cli/hold/).
- **Drift.** A unit with a current receipt shows its latest drift
  observation from [`nyl verify`](/nyl/reference/cli/verify/) in its detail
  (`drift: clean`, `drifted`, or `unknown`). A current unit whose latest
  observation is `drifted` has the state `drifted`: work for the next
  reconcile, like a changed unit.
- **Teardowns.** A unit pending teardown warns with `NYL-TEARDOWN-PENDING`,
  naming `nyl reconcile --allow-teardown` for an omission (or `nyl teardown
  --unit` again for an explicit request) and `nyl state forget` to keep its
  resources. A publication whose teardown waits warns with
  `NYL-TEARDOWN-WAITING`, naming `nyl teardown --unit <u> --confirm-removed`
  and `--allow-incomplete`.
- **Teardown readiness.** Units whose kind reports teardown settings, such
  as a `KubernetesPublication`, carry their teardown readiness in the
  report, `ready` or the unmet requirements, read from the stored desired
  document without rendering; `nyl get units -e <env> -o wide` shows it as a
  column.

`status` only reads state and needs no commit identity.

A run that ended without its transition commit, because it was cancelled or
lost its lease, leaves its results on its run ref until the next reconcile
imports them. Until then `status` reports it with `NYL-RUN-UNIMPORTED` and
shows the units it left uncertain as `uncertain`; a run that recorded every
unit only warns. A run whose lease expired or was broken before it finished
is reported with `NYL-LEASE-LOST`; the fix names `nyl reconcile`, and `nyl
recover -e <env>`, which imports the run without executing anything.

The table is separated from its indented source and commit provenance by a blank
line. Diagnostics follow separately, with a code, message and suggested fix in
each block.

When this clone's [local state branches](/nyl/orchestration/environments-and-state/#local-runs)
diverge from the configured state repository, `status` warns with
`NYL-STATE-LOCAL-DIVERGED`. Ahead/behind counts are Git commits, including retained
source ancestry; they do not count unit executions. The larger count across the
desired and observed refs is shown for each direction. A configured local bare
repository is still a separate state repository for this comparison.

The table shows the configured state repository unless `--local` is given.
`--local` shows this clone's branches, including `nyl reconcile --local` runs;
`--worktree` selects source inputs and does not select local state. The warning
does not change the exit code. Inspect the local view before using
[`nyl state reset --local`](/nyl/reference/cli/state/#nyl-state-reset), which can
discard local receipts.

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
| `--local` | Show this clone's local view of the state, including `--local` runs. Exits 1 with `NYL-LOCAL-RUN-UNSUPPORTED` for state without a remote, whose local branches already are the state. |
| `-o`, `--output <table\|json>` | A table, or the typed `StatusReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every unit is current. |
| 1 | An error, such as `NYL-STATE-MISSING`, or `NYL-STATE-DELETED` for state removed with `nyl state delete`. |
| 2 | Not everything is reconciled and nothing failed: a unit is ready, drifted, blocked, held, pending or waiting for its teardown, or awaiting approval. |
| 3 | A unit has a `failed` condition, also next to a lifecycle state, such as a failed teardown. |
| 4 | A unit is `uncertain`, a run lost its lease, or a cancelled run left units uncertain that no reconcile has imported yet (`NYL-RUN-UNIMPORTED`). |
