---
title: 'detect-drift and attest'
---

`nyl detect-drift` asks each unit's driver whether what its current receipt
applied still holds, and records the answers in the environment's state.
[`nyl attest`](#nyl-attest) asks the drivers that attest for their units to
attest the current receipts. Both check receipts without executing anything,
and share their options, leases, and exit codes. See
[Runs](/nyl/orchestration/runs/#execution-keys-and-receipts) for receipts and
when one is current.

Drift and attestations answer different questions. Drift says whether a unit
must execute again: a drifted unit is work for the next reconcile.
Attestations are evidence that dependents and promotion require, such as a
smoke test or a publication's Argo CD health; a `dependsOn` entry or
`fromUnit` [requires them](/nyl/orchestration/runs/#requiring-attestations)
with `evidence: attested` or `attestations`.

:::note
Orchestration is being built. `nyl attest` asks drivers to attest; recording
an attestation from a person or an external system, with
`nyl attest --name … --result …`, is not implemented yet.
:::

## nyl detect-drift

```bash
nyl detect-drift -e dev
nyl detect-drift -e dev --unit database
nyl detect-drift -e prod --wait-lease 10m -o json
```

`detect-drift` reads the environment's state, takes its lease, and calls the
driver's drift detection for every unit with a current receipt, each in a
checkout of the commit the unit executed, with the project directory located
at the unit's source commit as `reconcile` located it (following
`[project] previous_paths` after a move). It never executes a unit and never
writes a receipt. It writes one commit to the observed state ref with each
unit's drift record, `observed/units/<unit>/drift.yaml`: the result
(`clean`, `drifted` with a summary, or `unknown` with a reason), the
receipt's execution key, and the driver's details. A check that finds
nothing new writes no commit. Units are checked in batches of four; before
each batch, the lease's deadline moves to the batch's longest unit `timeout`
from then, plus the grace period, and no unit's check runs past the lease's
deadline. A check that passes its deadline fails with a timeout
(`NYL-DRIVER-ERROR`, exit 1), and nothing is recorded for that unit.

A unit without a current receipt is reported as `no-receipt`, and a unit
whose kind cannot detect drift as `unsupported`; neither is skipped silently.

A [`KubernetesPublication`](/nyl/orchestration/resource-guides/kubernetes-publication/#execution-key-and-result)
compares the publication branch with the tree it renders. When its target uses
`reconciler: {direct: {}}`, it also [compares the cluster](/nyl/kubernetes-gitops/workflow/direct-reconciler/#detecting-drift)
with what the unit last applied, with a server-side apply dry run as Nyl's field
manager, so a changed field Nyl manages or a deleted object is `drifted` and the
summary names the Release and the first eight objects. It needs the unit's
admitted `KUBECONFIG`, the Cluster's declared context, and `get`, `patch` (for the
dry runs), and access to the Release state Secrets, plus `get` on
customresourcedefinitions and `list` on apiservices when a kind is not served; a cluster it cannot read is
`unknown` (`NYL-DRIFT-UNKNOWN`, with the reason and the fix), never `clean`.
Run `nyl diff --target <target>` on a Release's file to see each change in full.

`detect-drift` and `attest` do not import runs, so they refuse a lease that
[`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break) broke while
the broken run is not imported yet, writing nothing; a check with nothing
to check needs no lease and does not refuse. The refusal comes before any
unit is checked, so its report lists no units:

```console
$ nyl detect-drift -e dev
dev: no units

  run:      01a17ac1-6115-73be-9b83-6ec5c43ada42

error NYL-LEASE-BROKEN
  the lease of dev was broken and its run 01a17ac1-5ed7-770a-9da3-a4fe0b96a2a1 has not been imported yet
  fix: run `nyl recover -e dev` to import it (a `nyl reconcile` or `nyl teardown` of dev imports it too), then run this command again
```

A drifted unit is `drifted` in [`nyl status`](/nyl/reference/cli/status/)
and work for the next [`nyl reconcile`](/nyl/reference/cli/reconcile/). What
that reconcile does depends on the environment's `driftRepair` and the
unit's recovery policy, and the `NYL-DRIFT-DETECTED` fix names it per unit:

| `driftRepair` | Recovery policy | Fix |
| --- | --- | --- |
| `converge` (default) | `converge` | the next `nyl reconcile -e <env>` repairs it by executing the unit again |
| `converge` | `inspect` or `manual` | it awaits approval: run `nyl reconcile -e <env> --approve <unit>` |
| `manual` | `converge` | repair it with `nyl reconcile -e <env> --unit <unit> --reapply --reason <text>` |
| `manual` | `inspect` or `manual` | repair it with `nyl reconcile -e <env> --unit <unit> --reapply --reason <text> --approve <unit>` |

A repair writes a new receipt and drops the unit's drift record in the
same commit; detect drift again to check the new receipt. See
[drift repair](/nyl/orchestration/resource-guides/environment/#drift-repair).

## nyl attest

```bash
nyl attest -e dev
nyl attest -e dev --unit smoke
nyl attest -e prod --wait-lease 10m -o json
```

`attest` asks the driver of every unit with a current receipt whose kind
attests for its spec, such as a [Command unit](/nyl/orchestration/resource-guides/command/)
with an `attest` step, in a checkout of the commit the unit executed, as
`detect-drift` does. It records each result as an attestation of the
receipt under its name in `observed/units/<unit>/attestations.yaml`, with
`source: {driver: attest}` and the driver's details, in one commit to the
observed state ref; an attestation that only its time changed keeps the
stored one, so a repeated `attest` writes nothing. Units whose kind does not
attest are left out of the report; one named with `--unit` is reported
`unsupported`. A driver that could not tell, such as an attest step that
failed, reports the unit `unknown` (`NYL-ATTEST-UNKNOWN`, exit 2) and records
nothing for it; the attestations recorded earlier stay.

A [`KubernetesPublication` that declares `observe: {}`](/nyl/orchestration/resource-guides/kubernetes-publication/#observing-argo-cd)
attests `accepted` and `healthy` by reading Argo CD, through the way its
ArgoCDInstance declares and the credential the unit admits, and lists each
Application's running publication and health in the details (`deployments`). A failing
attestation exits 3 (`NYL-ATTESTATION-FAILED`); an Argo CD that cannot be
read, a rejected or expired credential, or an unreadable publication branch is
`unknown`: exit 2 and nothing recorded. A stored attestation whose result is unchanged is rewritten with its new time once it is older than 15 minutes, so a repeated `attest` within that interval still writes nothing.

An attestation belongs to the receipt it names: a new receipt starts with
its attestations pending until `attest` runs again, or until its reconcile
step attests it, and the reconcile that records the new receipt drops the
earlier receipt's attestations. A unit that depends on an attestation stays
[blocked](/nyl/orchestration/runs/#requiring-attestations) until a pass is
recorded for the current receipt, and `attest` is what records it: the next
`reconcile` runs the blocked unit, and no `attest` is needed between the two
for a unit that attested itself in its `command`.

[`nyl observe`](/nyl/reference/cli/observe/) is the third check of current
receipts: it reads the observed values of units whose kind reads them, and
shares these options, leases, and exit codes.

## Cancellation and several environments

SIGINT or SIGTERM cancels the check: it starts no further batch,
forwards the signal to running tools, writes no observed commit, releases
the lease, and exits 4 with `NYL-RUN-CANCELLED`. A signal while
`--wait-lease` waits does the same, having written nothing. A second signal
ends the process at once.

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
| `--unit <name>`, `--units <a>,<b>` | Check only these units. A name the environment's state does not hold exits 1 with `NYL-UNIT-UNKNOWN`. |
| `--wait-lease <duration>` | Wait up to this long, such as `10m`, while another run holds the lease. |
| `--author "Name <email>"` | The author of the state commit. Default: Git's `user.name` and `user.email`. |
| `-o`, `--output <table\|json>` | A table of units, or the typed `DetectDriftReport` or `AttestReport`. Default `table`. |
| `[PATH]` | The project directory or a path beneath it. Default `.`. |

The table lists each unit with its check: for `detect-drift` `clean`,
`drifted`, `unknown`, `no-receipt`, or `unsupported`; for `attest`
`attested` with each attestation's result, such as `attested (smoke: pass)`,
`unknown`, `no-receipt`, or `unsupported`. `-o json` prints the same report
as JSON on stdout, with `check` per unit and, for `attest`, the result of
each attestation; diagnostics go to stderr.

The transition commit's summary records each unit's check, and its headline
counts them, such as `nyl: detect-drift dev (3 clean, 1 drifted)` or
`nyl: attest dev (1 attested)`.

## Codes

| Code | Meaning and fix |
| --- | --- |
| `NYL-DRIFT-DETECTED` | The unit's resources differ from what its receipt applied. The fix says what the next reconcile does with the unit: repairs it, waits for its approval, or, under `driftRepair: manual`, waits for `--reapply` (see the table above). |
| `NYL-DRIFT-NO-RECEIPT` | The unit has no current receipt to check for drift. Run `nyl reconcile`. |
| `NYL-DRIFT-UNKNOWN` | The driver could not tell whether the unit drifted, for example because its tool failed. Fix the cause and run `detect-drift` again. |
| `NYL-DRIFT-UNSUPPORTED` | The unit's kind cannot detect drift, such as a Command unit without a `detectDrift` step. |
| `NYL-ATTESTATION-FAILED` | An attestation of the unit's receipt failed. References that require the attestation stay unsatisfied until a newer one passes. |
| `NYL-ATTEST-NO-RECEIPT` | The unit has no current receipt to attest. Run `nyl reconcile`. |
| `NYL-ATTEST-UNKNOWN` | The driver could not tell the results of the unit's attestations. Fix the cause and run `attest` again. |
| `NYL-ATTEST-UNSUPPORTED` | `--unit` named a unit whose kind produces no attestations for its spec, such as a Command unit without an `attest` step. |
| `NYL-DRIVER-ERROR` | The check could not run, for example because the unit's commit could not be checked out, or it passed its `timeout` (the message reads `timeout: detect-drift passed …`). Fix the cause, or raise the unit's `timeout`, and run the command again. |
| `NYL-LEASE-BROKEN` | The lease was broken and its run is not imported yet. Run `nyl recover -e <env>`, then run the command again. |

## Exit codes

| Exit | Meaning |
| --- | --- |
| 0 | Every checked unit is clean (`detect-drift`), or every attestation passes (`attest`). |
| 1 | An error, such as `NYL-STATE-MISSING`, `NYL-UNIT-UNKNOWN`, `NYL-DRIVER-ERROR`, or `NYL-COMMIT-IDENTITY-MISSING`. |
| 2 | A unit could not be checked: it has no current receipt, its kind cannot run the check, or its result is unknown; or another run holds the lease (`NYL-LEASE-HELD`), or the lease is broken and its run not imported (`NYL-LEASE-BROKEN`). |
| 3 | A unit drifted (`detect-drift`), or one of its attestations failed (`attest`). |
| 4 | The lease was taken over before the commit (`NYL-LEASE-LOST`), or a signal cancelled the check (`NYL-RUN-CANCELLED`); nothing was written. |

When several apply, the most severe wins, in the order 4, 3, 1, 2, 0.

[`nyl reconcile`](/nyl/reference/cli/reconcile/) shares `-e`, `--unit`,
`--wait-lease`, `--author`, `-o`, and `[PATH]`, and uses the same exit
categories.
