---
title: 'recover'
---

Clears a unit's `uncertain` condition or non-retryable failure so the next
reconcile executes it again, or adopts the result a run checkpointed for it.
It records who decided and why. See [Recovery](/nyl/orchestration/recovery/)
for conditions, recovery policies, and takeover.

:::note
Orchestration is being built in milestone M3.
:::

```bash
nyl recover -e dev --unit migrate --retry --reason "verified nothing was applied"
nyl recover -e dev --unit database --accept 0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90 \
  --reason "the runner finished after losing its lease"
nyl recover -e dev --unit migrate --retry -o json
```

`recover` takes the environment's lease, reads its state, and writes one
commit to the observed state ref. It never executes anything:

- `--retry` clears the unit's condition and keeps its last receipt, so the
  unit is ready for the next [`nyl reconcile`](/nyl/reference/cli/reconcile/).
  An operator can decide locally while CI executes. A retryable failure
  needs no recover, because the next reconcile retries it, but `--retry`
  clears it too.
- `--accept <run>` adopts the receipt, outputs, and artifacts that run
  checkpointed for the unit, typically after a runner that lost its lease
  finished the unit late. It is allowed only when the checkpoint's execution
  key and uid equal the unit's current desired document's, so a checkpoint
  is accepted only for what the unit runs now.

Both refuse a unit without a condition. The report's `details` show the
unit's `condition` and the `evidence`: every checkpoint of the unit that a
run ref or evidence ref holds, newest run first, with its result
(`receipt`, `failed`, or `uncertain`), its execution key, and whether
`--accept` would adopt it.

The transition commit uses the operation `recover`, lists the unit under
`recovered`, records the action under `units`, such as
`database: {"action":"accept","run":"0192…"}`, and carries the
`Nyl-Requested-By` and `Nyl-Reason` trailers.

`recover` never deletes a run ref: a run that died without a transition
commit is taken over by the next reconcile, which imports its checkpoints.
After its commit, `recover` deletes the evidence refs, the checkpoints a
takeover kept for `uncertain` units, that no unit still uncertain needs.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment. Required. |
| `--unit <name>` | The unit to recover. Required. |
| `--retry` | Clear the condition; the next reconcile executes the unit. |
| `--accept <run>` | Adopt what this run checkpointed for the unit. Needs `--reason`. |
| `--reason <text>` | Why, recorded as the commit's `Nyl-Reason`. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease. |
| `--author "Name <email>"` | The author of the state commit, recorded as `Nyl-Requested-By`. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <text\|json>` | Human-readable lines, or the operation's `StateOperationReport`. Default `text`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

Exactly one of `--retry` and `--accept` is required.

## Codes

| Code | Meaning and fix |
| --- | --- |
| `NYL-RECOVER-NOTHING` | The unit has no uncertain or failed condition. Check it with `nyl status`. |
| `NYL-RECOVER-CHECKPOINT-MISSING` | The run has no checkpoint of the unit, its checkpoint holds a failure instead of a receipt, or `--accept` names no run ID. Pass a run the evidence lists as `receipt`, or use `--retry`. |
| `NYL-RECOVER-KEY-MISMATCH` | The run executed the unit with another execution key or uid than its current desired document. Use `--retry` to execute it again. |
| `NYL-UNIT-UNKNOWN` | The environment's state holds no such unit. |
| `NYL-RUN-CANCELLED` | SIGINT or SIGTERM arrived while `--wait-lease` waited; nothing was written. Once `recover` holds the lease, a signal lets it finish its single commit. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Done. |
| 1 | An error or refusal, such as one of the codes above or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | Another run holds the lease (`NYL-LEASE-HELD`). |

The [`nyl state`](/nyl/reference/cli/state/) commands share `-e`, `--reason`,
`--wait-lease`, `--author`, `-o`, and `[PATH]`, and use the same exit codes.
