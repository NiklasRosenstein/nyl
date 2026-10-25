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
`uncertain`, `failed`, `current`, `held`, `pending-teardown`, `tearing-down`,
or `retained`. Whether its latest receipt is still current is shown
separately, as `current`, `stale`, or `none`. For the documents behind a
state, use [`nyl get unit <name> -e <env>`](/nyl/reference/cli/project-resources/#inspect-environment-state).

`status` only reads state and needs no commit identity.

When this clone's [local state branches](/nyl/orchestration/environments-and-state/#local-runs)
hold local transitions or are behind the remote, `status` warns with
`NYL-STATE-LOCAL-DIVERGED`, such as "local state of dev has 2 local
transitions and is 3 behind the remote". The warning does not change the exit
code. `--local` shows the local view instead: the state as the clone's
branches hold it, including `nyl reconcile --local` runs.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment to show. Required. |
| `--local` | Show this clone's local view of the state, including `--local` runs. Exits 1 with `NYL-LOCAL-RUN-UNSUPPORTED` for state without a remote, whose local branches already are the state. |
| `-o`, `--output <table\|json>` | A table, or the typed `StatusReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every unit is current. |
| 1 | An error, such as `NYL-STATE-MISSING`. |
| 2 | Not everything is reconciled and nothing failed: a unit is blocked, held, or waiting. |
| 3 | A unit has a `failed` condition. |
| 4 | A unit is `uncertain`, or a run lost its lease. |
