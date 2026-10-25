---
title: 'Runs'
---

A run reconciles one environment: it renders the environment's units at one
source commit, resolves the references between them, executes the units whose
results are missing or out of date in dependency order, and records what
happened in one transition commit per state ref. [`nyl plan`](/nyl/reference/cli/plan/)
shows what a run would do, [`nyl reconcile`](/nyl/reference/cli/reconcile/)
performs it, and [`nyl status`](/nyl/reference/cli/status/) and
[`nyl get -e`](/nyl/reference/cli/project-resources/#inspect-environment-state)
read the result.

:::note
Runs cover one environment at a time and the [Command](/nyl/orchestration/resource-guides/command/)
unit kind today. Recovery commands, teardown, holds, approvals, and runs over
several environments follow in later parts of milestone M3.
:::

## Units

A unit is one step an environment runs, such as a [Command](/nyl/orchestration/resource-guides/command/).
Units are resources of `units.gitops.nyl/v1` anywhere in the project. Their
envelope, `apiVersion`, `kind`, `metadata.name`, and `metadata.labels`, is
literal, so an Environment's `unitSelector` selects them before anything
renders. Below the envelope, `spec` is a Nyl template rendered once per
selecting Environment, with the Environment's `values` as `values`.

```yaml
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: database
  labels: {tier: platform}
spec:
  command: ['./create-database.sh', '{{ values.size }}']
  outputs:
    host: {type: string}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: migrate
  labels: {tier: platform}
spec:
  values:
    host: {fromUnit: {unit: database, output: host}}
  command: ['./migrate.sh']
```

Unit names are unique per environment, and a unit's name is its identity for
references and receipts. `enabled: false`, or a document that renders to
nothing, takes a unit out of the environment.

Common fields every kind shares:

| Field | Meaning |
| --- | --- |
| `enabled` | `false` leaves the environment's ownership set. Default `true`. |
| `outputs` | The outputs the unit declares, each with a `type` and optionally `sensitive: true`. |
| `dependsOn` | Units that must have a current receipt first, for ordering without a data reference. |
| `timeout` | The deadline of one execution. |
| `env` | `passthrough`, the names of runner variables the unit may read. `secrets` is rejected with `NYL-UNIT-SECRETS-UNSUPPORTED` until secret admission arrives. |

## References between units

`{fromUnit: {unit: <name>, output: <output>}}` stands for an output of
another unit's current receipt, and `{fromUnit: {unit: <name>, artifact: <name>,
pointer: /…}}` for a value inside one of its artifacts. A reference replaces
the whole value it stands for, at any depth of the fields a kind accepts
references in, such as a Command's `values`. References add a dependency on
their producer, as `dependsOn` does. A cycle is rejected with
`NYL-DEPENDENCY-CYCLE`, and a reference to a unit the environment does not
select with `NYL-DEPENDENCY-UNKNOWN`.

A consumer whose producer has no current receipt is **blocked**: it keeps its
previous resolution and does not execute in this run. When the producer
executes in the same run, Nyl resolves again and the consumer runs in a later
wave. A sensitive output can never be referenced.

[`nyl validate`](/nyl/reference/cli/validate/) checks references without
state: the producer must exist and declare the output as non-sensitive.

## Source commit

Every run works at one source commit, S. The Environment's `source` decides
it: omitted, S is the commit of the worktree you run from; with `revision`, the
tip of that ref; with `revision` and `commit`, the pinned commit. `--source`
overrides S for one `plan` or `reconcile`; the transition commit marks the
override, and the next run uses the Environment's source again.

Nyl reads the Environment's `source`, `state`, `protectedRefs`,
`allowUnprotectedSource`, and `admission` fields, with the defaults of its
EnvironmentGroup, from the worktree you run from, because they decide S and
where state lives. Everything else, the units, their templates, and the
Environment's `values`, is rendered from a fresh checkout of S, so
uncommitted changes never reach a run.

- S must be reachable from one of the Environment's `protectedRefs`, by
  default the repository's default branch. `allowUnprotectedSource: true` lets
  an environment such as dev run from any commit, for example a feature
  branch.
- A pinned commit, of the Environment's `source` or of a unit's source fields,
  must be reachable from `protectedRefs` or from its own `revision`.
  `allowUnprotectedSource` never waives this.
- An unreachable commit fails the run with `NYL-SOURCE-UNREACHABLE`, naming
  the commit and the refs searched.

A Command unit runs from S. Kinds that build from Git name their files in
source fields with an optional `revision` and `commit`, which default to S. A
`revision` resolves to a commit during the run, and the unit's desired
document records that commit, so every execution can be reproduced from
state. All source fields of one unit must resolve to the same commit, its
effective commit; conflicting pins are rejected. Source fields naming another
repository are not supported yet.

## Execution keys and receipts

Each execution records a **receipt** in the observed state: the execution key
it ran with, its outputs, its artifacts, and the receipts it read through
references. A unit executes when it has no receipt whose key matches its
current execution key, or when a receipt it read is no longer current.

The execution key is a digest of everything that decides what the unit does:
its resolved spec, including the values its references resolved to, the
content its kind reads, such as a Command's `files` and `fingerprint`, and
the names of the variables it may read. A change outside those inputs, such
as a new commit that touches other files, never runs a unit again. A repeated
reconcile with no changes executes nothing and writes no commit.

## Waves

`nyl reconcile`:

1. takes the environment's [lease](/nyl/orchestration/environments-and-state/#where-state-lives);
2. resolves every unit at S against current receipts;
3. executes the ready units, independent units in parallel up to
   `--concurrency` (default 4), each in its own checkout of its effective
   commit, and checkpoints each result on the run ref;
4. resolves again at the same S, because new receipts unblock dependents, and
   repeats from step 3 until nothing is ready;
5. writes one transition commit per state ref, then releases the lease.

A failed execution records a condition and keeps the unit's previous receipt.
An execution that may have had effects without completing, such as one that
passed its timeout, is uncertain. `--unit` executes only the named units;
resolution still covers the whole environment, and a named unit whose
dependency has no current receipt stays blocked.

## Transition commits

A reconcile that changes state writes one commit to each state ref it
changes, and a reconcile that changes nothing writes none. The message
starts with a headline and a machine-readable summary of every unit's
result, followed by trailers naming the run, the source commit, the state
commits it read, the runner, and the evaluation time:

```text
nyl: reconcile dev (2 executed)

units:
  database: {result: executed, executionKey: sha256:…}
  migrate: {result: executed, executionKey: sha256:…}

Nyl-Operation: reconcile
Nyl-Run-Id: 0192…
Nyl-Environment: dev
Nyl-Source-Commit: 3e7b…
```

State commits are authored by `--author "Name <email>"`, else by Git's
`user.name` and `user.email`; without either, `nyl reconcile` exits 1 with
`NYL-COMMIT-IDENTITY-MISSING`. Commands that only read state, `plan`, `status`,
and `get`, need no identity.

## See also

- [Environments and state](/nyl/orchestration/environments-and-state/)
- [Using Command](/nyl/orchestration/resource-guides/command/)
- [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)
