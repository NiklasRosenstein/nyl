---
title: 'Using Command'
---

A Command unit runs a program from the environment's source commit, for a step
no built-in kind covers, such as seeding a database or registering a webhook.
Orchestration runs it when its execution key changes, records its outputs in
the unit's receipt, and lets other units read them with `fromUnit`.

```yaml
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: seed
spec:
  files: ['../scripts/seed/**']
  values:
    bucket: {fromUnit: {unit: storage, output: bucket}}
  command: ['./run.sh']
  verify: ['./verify.sh']
  fingerprint: ['./tool-versions.sh']
  workingDir: ../scripts/seed
  env:
    passthrough: [AWS_REGION, AWS_PROFILE]
  idempotent: true
  outputs:
    seedVersion: {type: string}
  timeout: 10m
```

## Fields

| Field | Meaning |
| --- | --- |
| `command` | The argument vector of the reconcile step. Required. |
| `verify` | An argument vector that reports drift: exit 0 for clean, 2 for drift, anything else for an error. |
| `teardown` | An argument vector that removes the command's effects. Without it the unit cannot tear down and defaults to `deletionPolicy: Retain`. |
| `fingerprint` | An argument vector whose stdout enters the execution key, such as the versions of the tools the command uses. |
| `files` | Local-path globs. The content of the files they match enters the execution key. |
| `values` | An object the command reads from `NYL_INPUTS`. References such as `fromUnit` may appear at any depth. |
| `workingDir` | The directory every step runs in. Default: the directory containing `nyl.toml`. |
| `idempotent` | Your claim that running the command again with the same inputs is safe. |
| `outputs` | The outputs the command declares, each with a `type` and optionally `sensitive: true`. |

The common unit fields, such as `dependsOn`, `approval`, `timeout`, `env`,
and `attestations`, work as for every unit.

Each step is an argument vector, executed directly without a shell. Write
`['sh', '-c', '…']` when you need one. A first element containing `/`, such
as `./run.sh`, resolves against the working directory and must stay inside
the worktree. A bare name, such as `python3`, is looked up on `PATH`.
`workingDir` follows the [local path rule](/nyl/reference/configuration/#local-paths):
relative to the `nyl.toml` directory with leading `..` allowed, or `/`-rooted
at the Git worktree, where `/` alone names the worktree root. It never leaves
the worktree or traverses a symbolic link.

Every step runs in a fresh, detached checkout of the source commit, so
uncommitted changes in the worktree you run Nyl from never reach it.

## Process environment

A step starts from an empty environment. Nyl sets only:

| Variable | Content |
| --- | --- |
| `PATH` | The runner's `PATH`. |
| `HOME` | A fresh, empty directory that exists only for the step. |
| `NYL_INPUTS` | The path of a JSON file `{"values": …}` holding the resolved `values`. |
| `NYL_OUTPUTS` | The path of an existing empty file for the step's outputs. Only `command` gets it. |
| `NYL_ATTESTATIONS` | The path of an existing empty file for attestation results, when the unit declares attestations it produces itself. |
| `NYL_UNIT` | The unit's name. A fingerprint runs before the unit's execution and does not get it. |
| `NYL_ENVIRONMENT` | The environment's name. |
| `NYL_OPERATION` | `reconcile`, `verify`, `teardown`, or `fingerprint`. |

The variables named in `env.passthrough` are copied from the runner's
environment. A missing declared variable stops the unit before it runs. Every other variable of the process that
runs Nyl stays out. The names in `env.passthrough` enter the execution key,
but their values do not, so a changed value does not run the command again.

There is no sandbox beyond the working directory and this environment. The
command runs with the runner's permissions.

### Secrets

Map environment variable names to keys in the project's secrets provider:

```yaml
env:
  passthrough: [AWS_REGION]
  secrets:
    DB_PASSWORD: database-password
```

String secrets pass through unchanged; structured values use compact JSON.
Secrets are available to execution, planning, verification, and teardown,
never to fingerprinting. Missing keys stop the call before a tool starts.
Transcripts mask their values, and state records only the declared names and
keys. Rotating a secret does not change the execution key. The provider is the
same one used by project templates; currently the built-in null provider
stores secrets in memory only.

## Outputs

The command writes its outputs to `NYL_OUTPUTS` as one JSON object:

```sh
#!/bin/sh
set -e
version=$(./seed --apply)
printf '{"seedVersion": "%s"}' "$version" > "$NYL_OUTPUTS"
```

- An empty file counts as `{}`, which is valid only when the unit declares no
  outputs. The file may hold at most 1 MiB.
- Every declared output must be present and have its declared type. A missing
  or mistyped output, or a file that is not one JSON object, fails the
  execution with category `output-rejected`.
- Only declared outputs are recorded. An undeclared output is ignored with the
  warning `NYL-OUTPUT-UNDECLARED`; declare it to record it.
- A `sensitive` output is checked like any other but never recorded, and other
  units cannot reference it.

Other units read a recorded output with
`{fromUnit: {unit: seed, output: seedVersion}}`.

Stdout and stderr form the transcript: Nyl prints them on stderr as the step
runs and never parses or stores them.

## Execution key

The command runs again when its execution key changes. The key covers the
steps, `workingDir`, `idempotent`, `outputs`, the resolved `values`, the names
in `env`, the content of the files `files` matches, and the stdout of
`fingerprint`.

The unit's desired document records `workingDir`, explicitly when omitted, and
each `files` glob in worktree-rooted form, such as `/scripts/seed`. Moving
`nyl.toml` while the files stay where they are therefore keeps the key, and the
command does not run again.

- Files outside `files` may be read but never run the command again. `files`
  never follows a symbolic link, and a glob matching one is an error.
- `fingerprint` runs during resolution with the same environment as the other
  steps. It must be fast and deterministic, and must exit 0; any other exit
  blocks the unit.

## Failures and retries

| How a step ends | Result |
| --- | --- |
| Exit 0 with a valid outputs file | Success: a receipt with the outputs |
| Any other exit code | Failure, category `tool-error` |
| Invalid outputs file | Failure, category `output-rejected` |
| The program cannot be started | Failure, category `missing-tool` |
| Passes its `timeout`, dies from a signal, or the run is cancelled | Uncertain: effects may have happened |

Every step has the unit's `timeout`, 30 minutes by default. At the deadline,
Nyl sends SIGTERM to the step's whole process group, including background
children, and SIGKILL 30 seconds later.

Failures are retried only with `idempotent: true`. An uncertain execution runs
again only with `idempotent: true` (recovery policy `converge`); otherwise it
waits for an operator to recover it (`manual`). Idempotency is your claim,
which Nyl cannot check.

A failure recorded without `idempotent: true` is not retried, even after you
fix the script, and `nyl status` says so. Once you have checked what the step
did, [`nyl recover -e <env> --unit <unit> --retry --reason <text>`](/nyl/reference/cli/recover/)
clears the failure or the uncertain execution, and the next reconcile runs the
unit again. [Recovery](/nyl/orchestration/recovery/) describes conditions and
their recovery in full.

`verify` records a drift observation, never a receipt. It receives the same
inputs and environment as `command`, without `NYL_OUTPUTS`. Under the
default `driftRepair: converge`, drift found by `nyl verify` makes an
idempotent command execute again in the next reconcile, which lists it under
`driftRepaired`. A command without `idempotent: true` is not safe to re-run:
reconcile reports it `awaiting-approval` instead. See
[drift repair](/nyl/orchestration/resource-guides/environment/#drift-repair). `teardown`
receives the stored `values` under `values` and the last receipt's outputs
under `outputs` in `NYL_INPUTS`; exit 0 completes the deletion.

## See also

- [Runs](/nyl/orchestration/runs/) and [`nyl reconcile`](/nyl/reference/cli/reconcile/)
- [Environments and state](/nyl/orchestration/environments-and-state/)
- [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)
