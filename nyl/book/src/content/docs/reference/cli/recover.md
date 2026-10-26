---
title: 'recover'
---

Imports the run of a broken lease without executing anything, clears a
unit's `uncertain` condition or non-retryable failure so the next reconcile
executes it again, or adopts the result a run checkpointed for it. It
records who decided and why. See [Recovery](/nyl/orchestration/recovery/)
for conditions, recovery policies, takeover, and breaking a lease.

:::note
Orchestration is being built in milestone M3.
:::

```bash
nyl recover -e dev --reason "the CI runner was killed"
nyl recover -e dev --unit migrate --retry --reason "verified nothing was applied"
nyl recover -e dev --unit database --accept 0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90 \
  --reason "the runner finished after losing its lease"
nyl recover -e dev --unit migrate --retry -o json
```

`recover` takes the environment's lease and writes one transition commit.
It never executes anything, and it writes no run record of its own: its
commit is all it writes.

## Importing a broken run

After [`nyl lease break`](/nyl/reference/cli/state/), every command that
writes state refuses with `NYL-LEASE-BROKEN` until an operation imports the
broken holder's run. `nyl recover -e <env>` is that import when no
reconcile should run yet. It takes the broken lease over and imports the
run the way a reconcile takeover does, through the same run path with
execution switched off:

- It resolves the environment's units at the source, adopts the uids of
  units the dead run created, turns checkpoints whose execution key matches
  into `imported` receipts, and gives units the run left without a
  checkpoint an `uncertain` condition. The next `nyl reconcile` runs those
  again under their recovery policy.
- The commit uses the operation `recover`, lists the import under
  `imported` and `recovered`, and records the break once under
  `leaseBroken`, with who broke the lease and why. It is written even when
  the broken run left nothing to import, such as a broken `nyl hold`.
- It consumes the broken run's refs as a takeover does. If `recover` ends
  before its commit lands, it puts the broken lease back, and the run's
  refs stay for the next import.
- It exits 0 once the import is recorded, also when units are left
  `uncertain`; `nyl status` then exits 4 for them.

With `--unit`, the `--retry` or `--accept` action applies after the import,
in the same commit, so an operator can import and decide a unit in one
command.

Where state cannot be imported into, `recover -e <env>` still clears the
break, so the command that repairs the state can run:

| State | What `recover -e <env>` does | Then run |
| --- | --- | --- |
| No `state.yaml`, such as after a broken `nyl state init` | Takes the lease over and releases it | `nyl state init` |
| Moved away (`NYL-STATE-MOVED`) | Records the break in an unchanged commit and releases the lease | `nyl state move`, or point the Environment at the new location |
| A `Nyl-Local` commit at a remote tip (`NYL-STATE-LOCAL-TIP`) | Releases the lease without committing on that tip | `nyl state restore` |

It reports the state's finding as a warning. With `--unit`, `recover`
refuses in these states with that finding instead.

Without a broken lease and without `--unit`, `recover` refuses with
`NYL-RECOVER-NOTHING`.

## Recovering a unit

- `--retry` clears the unit's condition and keeps its last receipt, so the
  unit is ready for the next [`nyl reconcile`](/nyl/reference/cli/reconcile/).
  An operator can decide locally while CI executes. A retryable failure
  needs no recover, because the next reconcile retries it, but `--retry`
  clears it too. On a unit held with `nyl hold` it clears only
  the condition; the unit stays held.
- `--accept <run>` adopts the receipt, outputs, and artifacts that run
  checkpointed for the unit, typically after a runner that lost its lease
  finished the unit late. It is allowed only when the checkpoint's execution
  key and uid equal the unit's current desired document's, so a checkpoint
  is accepted only for what the unit runs now. A held unit runs nothing, so
  `--accept` refuses it: resume the unit first. The accepted receipt keeps
  a copy of the desired document it was produced for.

Both refuse a unit without a condition. The report's `details` show the
unit's `condition` and the `evidence`: every checkpoint of the unit that a
run ref or evidence ref holds, newest run first, with its result
(`receipt`, `failed`, or `uncertain`), its execution key, and whether
`--accept` would adopt it.

The transition commit uses the operation `recover`, lists the unit under
`recovered`, records the action under `units`, such as
`database: {"action":"accept","run":"0192…"}`, and carries the
`Nyl-Requested-By` and `Nyl-Reason` trailers.

Apart from importing a broken run, `recover` never deletes a run ref: a run
that died without a transition commit is taken over by the next reconcile,
which imports its checkpoints. After its commit, `recover` deletes the
evidence refs, the checkpoints a takeover kept for `uncertain` units, that
no unit still uncertain needs.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment. Required. |
| `--unit <name>` | The unit to recover; needs `--retry` or `--accept`. Without it, `recover` imports the run of a broken lease. |
| `--retry` | Clear the condition; the next reconcile executes the unit. |
| `--accept <run>` | Adopt what this run checkpointed for the unit. Needs `--unit` and `--reason`. |
| `--reason <text>` | Why, recorded as the commit's `Nyl-Reason`. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease. |
| `--author "Name <email>"` | The author of the state commit, recorded as `Nyl-Requested-By`. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <text\|json>` | Human-readable lines, or the operation's `StateOperationReport`. Default `text`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

`--unit` takes exactly one of `--retry` and `--accept`, and each of them needs `--unit`.

## Codes

| Code | Meaning and fix |
| --- | --- |
| `NYL-RECOVER-NOTHING` | The unit has no uncertain or failed condition, or, without `--unit`, the lease is not broken, so there is no run to import. Check the environment with `nyl status`. |
| `NYL-RECOVER-CHECKPOINT-MISSING` | The run has no checkpoint of the unit, its checkpoint holds a failure instead of a receipt, or `--accept` names no run ID. Pass a run the evidence lists as `receipt`, or use `--retry`. |
| `NYL-RECOVER-KEY-MISMATCH` | The run executed the unit with another execution key or uid than its current desired document, or the unit is held. Use `--retry` to execute it again, or `nyl resume` a held unit first. |
| `NYL-UNIT-UNKNOWN` | The environment's state holds no such unit. |
| `NYL-LEASE-HELD` | Another run holds the lease. Wait for it, or pass `--wait-lease`. |
| `NYL-RUN-CANCELLED` | SIGINT or SIGTERM arrived while `--wait-lease` waited; nothing was written. Once `recover` holds the lease, a signal lets it finish its single commit. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Done, including an import that left units `uncertain`. |
| 1 | An error or refusal, such as one of the codes above or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | Another run holds the lease (`NYL-LEASE-HELD`). |

The [`nyl state`](/nyl/reference/cli/state/) commands share `-e`, `--reason`,
`--wait-lease`, `--author`, `-o`, and `[PATH]`, and use the same exit codes.
