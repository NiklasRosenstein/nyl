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

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment to show. Required. |
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
