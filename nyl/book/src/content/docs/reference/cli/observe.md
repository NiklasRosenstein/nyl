---
title: 'observe'
---

`nyl observe` reads the [observed values](/nyl/orchestration/runs/#observed-values)
of an environment's units and records them, without executing anything. It is
for jobs that hold the read credentials a [`reconcile`](/nyl/reference/cli/reconcile/)
may lack, such as access to a workload cluster that publishing does not need:
a scheduled job can keep a value current, and `reconcile` then resolves its
consumers from the record. It is a check of current receipts like
[`detect-drift` and `attest`](/nyl/reference/cli/detect-drift/), and shares
their options, leases, and exit codes. See [Runs](/nyl/orchestration/runs/#execution-keys-and-receipts)
for receipts and when one is current.

```bash
nyl observe -e dev
nyl observe -e dev --unit kubernetes
nyl observe -e prod --wait-lease 10m -o json
```

`observe` reads the environment's state, takes its lease, and calls the driver's
`observe` for every unit that has a current receipt, whose kind reads observed
values for its spec, and that declares `observations`, each in a checkout of the
commit the unit executed, with the project directory located at the unit's source
commit as `reconcile` located it. It never executes a unit and never writes a
receipt. It writes one commit to the observed state ref, operation `observe`,
with each unit's observation record, `observed/units/<unit>/observations.yaml`:
the value or the `pending` reason of each observation, for the receipt's
execution key. A read that changes nothing writes nothing, and a recorded value
is written again only after an hour (half of its `maxAge` when that is shorter), so
a scheduled `observe` does not commit on every run. A read that failed keeps the
last recorded value for the receipt.

A unit without a current receipt is reported as `no-receipt`; a unit named with
`--unit` whose kind cannot read observed values for its spec, or that declares
no observations, as `unsupported`. Neither is skipped silently, and a
unit that is not named and observes nothing is left out of the report.

```console
$ nyl observe -e dev
UNIT        KIND                   OBSERVATIONS                          DETAIL
kubernetes  KubernetesPublication  observed (ingressIp: "203.0.113.30")

  run:      00000000-0000-7000-9000-000000000004
  observed: 85c5967c6e8f862874a48520505d0824eb5b98cf

warning  observed value ingressIp of kubernetes changed, so dns resolves another value than it executed with

  Action  run `nyl reconcile -e dev`: it executes dns with the new value, under its own approval

  NYL-OBSERVATION-CHANGED
```

An observed value that changed is not drift and not evidence: the producer's
receipt still holds. Its consumers resolved another value, though, so they are
work for the next reconcile. `observe` names each of them with
`NYL-OBSERVATION-CHANGED`, whose fix is that reconcile, and so does [`nyl
status`](/nyl/reference/cli/status/) (the consumer is `ready`, exit 2). The
same holds for a consumer that was blocked on a value that has arrived now. The
transition commit's summary lists the changed observations under
`observationsChanged`, such as `observationsChanged: {kubernetes: [ingressIp]}`
and its headline counts the units, such as `nyl: observe dev (1 observed)`.

`observe` does not apply a consumer's changed inputs: that reconcile executes
it, under its own `approval`, as it does for any changed input.

Like `detect-drift`, `observe` does not import runs, so it refuses a lease that
[`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break) broke while the
broken run is not imported yet, writing nothing. A signal cancels it
(`NYL-RUN-CANCELLED`, exit 4) without writing an observed commit. Select several
environments with repeated `-e`, with `-l`, or with `-g`, as for the other
checks.

## Options

`observe` takes the options of the other checks:

| Option | Meaning |
| --- | --- |
| `-e`, `--environment <name>` | Select named environments; repeatable. Use exactly one of `-e`, `-l`, or `-g`. |
| `-l`, `--labels <key=value>` | Select declared environments by literal labels. |
| `-g`, `--group <name>` | Select the members of an EnvironmentGroup. |
| `--unit <name>`, `--units <a>,<b>` | Observe only these units. A name the environment's state does not hold exits 1 with `NYL-UNIT-UNKNOWN`. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease. |
| `--author "Name <email>"` | The author of the state commit. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table of units, or the typed `ObserveReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

The table lists each unit with `observed` and every observation's read, such as
`observed (ingressIp: "203.0.113.10", port: pending)`, or `no-receipt` or
`unsupported`. `-o json` prints the report on stdout, with `check` per unit and
each observation as `{observation: value | pending | failed, value | reason}`;
diagnostics go to stderr.

## Codes

| Code | Meaning and fix |
| --- | --- |
| `NYL-OBSERVATION-PENDING` | The system holds no value for an observation of the unit's current receipt yet; its consumers stay blocked. Run `observe` again once it does. |
| `NYL-OBSERVATION-FAILED` | A read failed, for example for a credential the unit does not admit, or an unreachable cluster. The last recorded value for the receipt stays usable. Fix the cause and run `observe` again. |
| `NYL-OBSERVATION-CHANGED` | An observed value changed or arrived, so its consumers are ready for the next `nyl reconcile`. |
| `NYL-OBSERVE-NO-RECEIPT` | The unit has no current receipt to observe. Run `nyl reconcile`. |
| `NYL-OBSERVE-UNSUPPORTED` | `--unit` named a unit that declares no observations, or whose kind reads none for its spec. |
| `NYL-DRIVER-ERROR` | The read could not run, for example because the unit's commit could not be checked out, or it passed the unit's `timeout`. Fix the cause and run again. |
| `NYL-LEASE-BROKEN` | The lease was broken and its run is not imported yet. Run `nyl recover -e <env>`, then run the command again. |

A [publication](/nyl/orchestration/resource-guides/kubernetes-publication/#observed-values)
adds `NYL-KUBERNETES-OBSERVATION-UNOWNED` at resolution, and puts its own words
in the reasons it reports: `application-not-running-publication` and
`field-not-set` for a value that is pending.

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every observation of every observed unit has a value. |
| 1 | An error, such as `NYL-STATE-MISSING`, `NYL-UNIT-UNKNOWN`, `NYL-DRIVER-ERROR`, or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | An observation is `pending` or `failed`; a unit has no current receipt or cannot be observed; or another run holds the lease (`NYL-LEASE-HELD`) or the lease is broken and its run not imported. |
| 4 | The lease was taken over before the commit (`NYL-LEASE-LOST`), or a signal cancelled the command (`NYL-RUN-CANCELLED`); nothing was written. |

When several apply, the most severe wins, in the order 4, 1, 2, 0.
