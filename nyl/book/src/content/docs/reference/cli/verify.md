---
title: 'verify'
---

Asks each unit's driver whether what its current receipt applied still
holds, and records the answers in the environment's state. See
[Runs](/nyl/orchestration/runs/#execution-keys-and-receipts) for receipts and
when one is current.

:::note
Orchestration is being built in milestone M3. `verify` covers one
environment at a time; verifying several environments in one invocation
follows with multi-environment runs.
:::

```bash
nyl verify -e dev
nyl verify -e dev --unit database
nyl verify -e prod --wait-lease 10m -o json
```

`verify` reads the environment's state, takes its lease, and calls the
driver's verification for every unit with a current receipt, each in a
checkout of the commit the unit executed. It never executes a unit and never
writes a receipt. It writes one commit to the observed state ref with the
latest observation per type (`drift`, and `health` where the driver reports
one) under `observed/units/<unit>/observations/`, and the driver's
attestations of the receipt under `observed/units/<unit>/attestations/`.
A verification that finds nothing new writes no commit. The lease's
deadline follows the longest `timeout` of the units it verifies.

A unit without a current receipt is reported as `no-receipt`, and a unit
whose kind cannot verify as `unsupported`; neither is skipped silently.

## Options

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | The environment to verify. Required. |
| `--unit <name>`, `--units <a>,<b>` | Verify only these units. A name the environment's state does not hold exits 1 with `NYL-UNIT-UNKNOWN`. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease. |
| `--author "Name <email>"` | The author of the state commit. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table of units, or the typed `VerifyReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

The table lists each unit with its verification: `clean`, `drifted`,
`unknown`, `no-receipt`, or `unsupported`. `-o json` prints the same report
as JSON on stdout, with `verification` per unit and the result of each
attestation; diagnostics go to stderr.

The transition commit's summary records each unit's verification, and its
headline counts them, such as `nyl: verify dev (3 clean, 1 drifted)`.

## Codes

| Code | Meaning and fix |
| --- | --- |
| `NYL-VERIFY-DRIFT` | The unit's resources differ from what its receipt applied. Run `nyl plan --unit <u>` to see the change and `nyl reconcile` to apply the declared state, or investigate the change made outside Nyl. |
| `NYL-ATTESTATION-FAILED` | A check the driver attests failed. References that require the attestation stay unsatisfied until a newer one passes. |
| `NYL-VERIFY-NO-RECEIPT` | The unit has no current receipt to verify. Run `nyl reconcile`. |
| `NYL-VERIFY-UNKNOWN` | The driver could not tell whether the unit drifted, for example because its tool failed. Fix the cause and verify again. |
| `NYL-VERIFY-UNSUPPORTED` | The unit's kind cannot verify, such as a Command unit without a `verify` step. |
| `NYL-DRIVER-ERROR` | The verification could not run, for example because the unit's commit could not be checked out. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every verified unit is clean and its attestations pass. |
| 1 | An error, such as `NYL-STATE-MISSING`, `NYL-UNIT-UNKNOWN`, `NYL-DRIVER-ERROR`, or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | A unit could not be verified: it has no current receipt, its kind cannot verify, or its drift is unknown; or another run holds the lease (`NYL-LEASE-HELD`). |
| 3 | A unit drifted, or one of its attestations failed. |
| 4 | The lease was taken over before the commit (`NYL-LEASE-LOST`); nothing was written. |

When several apply, the most severe wins, in the order 4, 3, 1, 2, 0.

[`nyl reconcile`](/nyl/reference/cli/reconcile/) shares `-e`, `--unit`,
`--wait-lease`, `--author`, `-o`, and `[PATH]`, and uses the same exit
categories.
