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
Runs support the [Command](/nyl/orchestration/resource-guides/command/) unit
kind, multi-environment dependencies, approvals, [recovery](/nyl/orchestration/recovery/)
from runs that died, drift repair, and [deletion and holds](/nyl/orchestration/deletion/).
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
| `observations` | The [observed values](#observed-values) the unit declares, for kinds that read them, each with a `type`, an optional `maxAge`, and the kind's own `from`. |
| `observationWait` | How long a run that executed the unit [waits](#observed-values) for its pending observed values, within `timeout`. Default `0s`. |
| `dependsOn` | Units that must have a current receipt first, for ordering without a data reference. |
| `timeout` | The deadline of one execution, at most `7d`. |
| `env` | `passthrough`, the names of runner variables the unit may read, and `secrets`, mapping variable names to project secret keys. Missing credentials stop the unit before execution. |

## References between units

`{fromUnit: {unit: <name>, output: <output>}}` stands for an output of
another unit's current receipt, `{fromUnit: {unit: <name>, artifact: <name>,
pointer: /…}}` for a value inside one of its artifacts, and
`{fromUnit: {unit: <name>, observation: <name>}}` for one of its
[observed values](#observed-values). A reference replaces
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
state, as `plan` and `reconcile` do before reading it: the producer must exist
and declare the output as non-sensitive. An artifact reference is checked only
for the producer's existence.

### Requiring attestations

A receipt says a unit ran, not that what it did works. A `dependsOn` entry or a
`fromUnit` reference can require more of its producer with `evidence` and
`attestations`:

```yaml
dependsOn:
  - {unit: kubernetes, attestations: [healthy]}   # smoke tests run against healthy workloads
values:
  endpoint: {fromUnit: {unit: service, output: endpoint, evidence: attested}}
```

- `evidence: published` (the default) requires a current receipt.
  `evidence: attested` also requires every attestation the producer declares to
  pass, and a producer that declares none is satisfied by its receipt.
- `attestations: [a, b]` requires exactly those, whatever `evidence` says. Each
  must be one the producer declares: the names its kind produces, such as
  `accepted` and `healthy` for a [publication that declares
  `observe: {}`](/nyl/orchestration/resource-guides/kubernetes-publication/#observing-argo-cd), and the
  names in its `spec.attestations`. Naming another is an error
  (`NYL-ATTESTATION-UNDECLARED`), which `nyl validate` reports too.
- An attestation declared with `from: external` is supplied only by a person
  or an external system, and Nyl cannot record those yet
  (`nyl attest --name … --result …` arrives with promotion). Requiring one,
  by name or through `evidence: attested` on a producer that declares one, is
  an error (`NYL-ATTESTATION-EXTERNAL-UNSUPPORTED`) rather than a consumer
  that waits forever; require the names the producer's driver observes
  instead. `nyl validate` reports it for producers in the same environment;
  for another environment's producer, `plan` and `reconcile` do.
- An attestation counts only for the receipt it names. A producer that
  executes again starts with its attestations pending, and a pass recorded
  for the earlier receipt no longer counts.
- A consumer whose producer has a current receipt but lacks the evidence is
  **blocked**, exit 2, and reports why: `attestation-pending` when nothing is
  recorded for the receipt, `attestation-failing` when the recorded result is
  `fail`, with the producer and the attestation, and a warning with the fix
  (`NYL-ATTESTATION-PENDING` or `NYL-ATTESTATION-FAILING`). `plan` and `status`
  show the same blocker.

Attestations are recorded by [`nyl attest`](/nyl/reference/cli/detect-drift/#nyl-attest),
typically in CI where the credentials exist, and by a unit that attests itself
in its `reconcile`, such as a [Command](/nyl/orchestration/resource-guides/command/)
whose `command` writes `NYL_ATTESTATIONS`. Every `reconcile` resolves again, so
a blocked consumer runs in the first run after a pass is recorded, or in a later
wave of the run in which its producer attested itself. Requiring an attestation
adds no age limit: the recorded pass for the current receipt is enough, and a
re-verified system simply keeps it fresh. A consumer that already ran stays
`current` when the producer is attested again after executing, provided its own
inputs did not change.

A `dependsOn` entry gates a consumer but gives it no input, so a consumer with
only `dependsOn` does not run again when only its producer's receipt changes:
once the new receipt has its attestations, the consumer is `current` and
nothing executes. To run a smoke test against each new version, make the
version an input: read an output or artifact field of the producer that
changes with it, such as its version, image digest, or endpoint, with
`fromUnit`. When the value changes, the consumer's execution key changes and
it runs, after the new receipt's attestations pass if the reference requires
them:

```yaml
values:
  version: {fromUnit: {unit: service, output: version, attestations: [healthy]}}
``` The attestation `maxAttestationAge` limit belongs to
promotion paths.

### Observed values

An output is produced by executing a unit. Some facts only appear after, such
as the address a cloud load balancer assigns to a published Service, and can
change while the unit's execution key stays the same. A unit whose kind reads
such facts declares them under `observations`, and its driver reads them from
the system the unit manages:

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata: {name: kubernetes}
spec:
  target: '{{ values.target }}'
  observe: {}
  observations:
    ingressIp:
      type: string
      from:                 # the kind's own source; here a published resource's field
        resource: {apiVersion: v1, kind: Service, namespace: ingress, name: ingress-nginx}
        pointer: /status/loadBalancer/ingress/0/ip
  observationWait: 10m
  env: {passthrough: [KUBECONFIG]}
---
apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: dns}
spec:
  command: ['./dns.sh']
  values:
    address: {fromUnit: {unit: kubernetes, observation: ingressIp}}
```

`type` is one of the output types, checked on every read; `maxAge` is how old
the last successful read may get before the value stops resolving. What `from`
means belongs to the kind: its driver validates it and Nyl never reads it. A
kind that reads no observed values rejects `observations`
(`NYL-OBSERVATIONS-UNSUPPORTED`), and an observation is never sensitive.

Observed values are committed in plain text to the state repository, where
everyone who can read it can read them: observe only non-secret fields. A kind
may reject some sources (a publication never reads Secrets or a whole
resource), but cannot tell what a ConfigMap or a custom resource holds.

An observed value is an observation of the producer's *current receipt*, never
part of it. It is recorded in the observed state
(`observed/units/<unit>/observations.yaml`), and the commit that records a new
receipt, a re-execution with the same key included, removes the previous receipt's values, so a value read from what ran
before never satisfies a reference.

- **When Nyl reads.** `reconcile` reads every unit with a current receipt that
  declares observations once, before it resolves the unit's consumers,
  including a unit it does not execute: this is how an address that changed by
  itself reaches its consumers in one run. A run that executes a unit with an
  `observationWait` then reads again about every 15 seconds, until every value
  has a value or the wait has passed since the execution finished (within the
  unit's `timeout`). It waits only when no other unit is ready, so units that do
  not depend on the value are not held back. With the default `0s` a run reads
  once, and a value that is not there stays `pending` for a later run.
  [`nyl observe`](/nyl/reference/cli/observe/) reads and records without
  executing anything, for jobs that hold read credentials a reconcile may
  lack.
- **Pending, failed, stale.** A read finds a value, `pending` (the system has
  not caught up, with the driver's reason), or `failed` (it could not read, such
  as for missing credentials). A consumer of a pending value is **blocked**,
  exit 2, with `NYL-OBSERVATION-PENDING` and the reason. A failed read keeps the
  last recorded value for the receipt and warns with `NYL-OBSERVATION-FAILED`; a
  value older than its `maxAge` is pending for its consumers.
- **A changed value is work for the next reconcile.** It is neither drift nor
  evidence: the producer's receipt still holds, and `driftRepair` does not
  apply. The value enters its consumers' execution keys, so each of them runs
  again like for any changed input, under its own
  [`approval`](#approvals), while the producer does not execute. `nyl status`
  and `nyl plan` show such a consumer as `ready`, and `nyl observe` names it
  with `NYL-OBSERVATION-CHANGED`. They find consumers of the same environment
  from its stored documents: a consumer in another environment (a reference with
  `environment`) is not named, and stays `current` in its own `status` until its
  next `reconcile` resolves the producer environment's recorded value and runs it.
- **What is written.** The record changes only when a value arrives, changes,
  or becomes pending again, or when the last successful read is older than an
  hour (half the `maxAge` when that is shorter), so an unchanged read writes
  nothing. The transition commit lists the observations that changed under
  `observationsChanged`.

`nyl get observation <unit>/<name> -e <env>` prints the recorded value, resolved
like the reference would. References to another declared environment read the
other environment's recorded values.

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

A Command unit runs from S. Each unit source field can name its own `revision`
and `commit`, in this
repository or a declared `GitRepository` (or inline `repository`). A remote
source requires `revision`; its paths are relative to that checkout's root.
Local source paths stay relative to the directory containing `nyl.toml`, or
worktree-rooted with `/`. Named image contexts may therefore read different
commits and repositories in one build. Resolution records each field's commit
and normalized path, and reads the same bytes when fingerprinting, planning,
executing, checking for drift or attesting, and tearing down.

Image and infrastructure drivers declare the paths they read. Their private
checkouts contain those paths, including declared local modules and variable
files, plus this repository's configuration directory (`nyl.toml` alone when
it sits at the repository root). Unrelated repository files
are not checked out. Command units retain a full checkout because their
`files` list determines freshness without restricting everything the command
may read. Changing a source commit alone does not re-run a unit when its
recorded input bytes are unchanged.

## Worktree inputs

For local development, reconcile uncommitted declarations and input files with:

```bash
nyl reconcile -e dev --worktree
```

Nyl snapshots Git-visible tracked and untracked files once, preserving deletions
and executable modes. Ignored untracked files and the configured Nyl cache are excluded.
Tracked files inside the cache cause capture to fail; move the cache or untrack them. Internal symlinks
are preserved without following their targets; input paths cannot traverse
symlinks. HEAD and the staging index stay untouched. Discovery, preflight,
rendering, and all execution waves use that immutable snapshot; subsequent edits
belong to the next run. Explicitly pinned unit sources keep their declared
commits. Drivers may require committed source; `--worktree` refuses those units.
A source input that needs a submodule or a Git LFS pointer cannot be
reconstructed from the snapshot; vendor those files as ordinary Git content.
Direct worktree operations reject required LFS pointer files; hydrate local file
inputs or vendor Git source files as ordinary Git content. The flag requires exactly one named environment and
conflicts with `--source`, label selectors, and group selectors. Capture progress on
stderr shows the base revision and file scope, then whether the snapshot was
created or reused, its commit, elapsed time, local ref, and state history location.
The state history retains it when the run records state. Structured
output remains on stdout.

State and run checkpoints archive the snapshot, including Git-visible uncommitted
files, in the configured state repository. The source base is recorded as provenance
without retaining its ancestry. Recovery and
teardown can retrieve those inputs from another clone after the working files
are gone. Choose a local state repository for disposable experiments, as in the
[local Docker example](https://github.com/NiklasRosenstein/nyl/tree/develop/examples/oci-image-opentofu).
`--local` independently uses the configured state repository's local copy without
contacting the remote.

`NYL-SOURCE-WORKTREE`, the transition's `worktreeSource` summary, and the reserved
`refs/nyl-worktrees/` source revision identify the run's provenance. Those refs
cache snapshots locally; the state history retains the archived inputs. Nyl does
not automatically garbage collect snapshots or compact that history. Snapshots
cannot publish Kubernetes trees or count as clean committed-source promotion
evidence. Commit changes and reconcile without the flag for that evidence.

## Following execution

Reconcile prints a start and end message for every dispatched unit. End messages
include the result and elapsed time, with a short reason when the unit fails.
Public tool output streams live, indented and labelled with the environment/unit
address, so concurrent units remain distinguishable. Terraform and OpenTofu show
their readable init, plan and apply output; structured version, plan inspection
and output-value JSON stays private. Admitted secrets are masked before display
or logging. Progress and tool transcripts go to stderr; JSON reports stay on stdout.

Use `--quiet-unit-output` to hide tool output while keeping unit start/end
messages. `--unit-log-dir DIR` also appends the masked transcript to
`DIR/<environment>/<unit>.log`; combine the flags to write files without showing
tool output. These files contain public tool output, never private structured
stdout. Log files append across invocations, and new files have private
permissions on Unix. `NYL-UNIT-LOG` warns when a log cannot be opened or written;
the tool result remains authoritative. Logs are local files and are not stored in Nyl state.

```bash
nyl reconcile -e dev --unit-log-dir /tmp/nyl-logs
nyl reconcile -e dev --quiet-unit-output --unit-log-dir /tmp/nyl-logs
```

The final summary follows dispatch order, with names breaking ties in a parallel
wave. Units that did not run follow in dependency order, and teardown lists
consumers before their producers. JSON reports provide this order in `unitOrder`
alongside the `units` map.

## Execution keys and receipts

Each execution records a **receipt** in the observed state: the execution key
it ran with, its outputs, its artifacts, and the receipts it read through
references. A unit executes when it has no receipt whose key matches its
current execution key, or when a receipt it read is no longer current. The
execution key alone decides: a producer recreated with a new identity and the
same outputs leaves its consumers current, and a unit that must run again
whenever a producer is recreated says so in its inputs, for example by
reading an output that changes with every creation. A unit whose latest
drift record is `drifted` runs again too, as
[Recovery](/nyl/orchestration/recovery/#repairing-drift) describes.

The execution key is a digest of everything that decides what the unit does:
its resolved spec, including the values its references resolved to, the
content its kind reads, such as a Command's `files` and `fingerprint`, and
the names of the variables it may read. A change outside those inputs, such
as a new commit that touches other files, never runs a unit again. A repeated
reconcile with no changes executes nothing and writes no commit.

Execution progress and the reconcile report explain why each dispatched unit
runs. A changed driver behavior version is identified when the receipt records it;
other key differences are reported as changed execution inputs. The complete earlier input manifest is not stored.
Status and reconcile show a different Nyl version recorded in a receipt as
provenance. Nyl version is not an execution key input and does not make a
receipt stale or trigger execution.

## Waves

`nyl reconcile`:

1. takes the environment's [lease](/nyl/orchestration/environments-and-state/#where-state-lives);
2. resolves every unit at S against current receipts;
3. executes the ready units, independent units in parallel up to
   `--concurrency` (default 4), and checkpoints each result on the run ref.
   Each of the `--concurrency` slots keeps one checkout for the run and resets
   it to the next unit's effective commit, removing untracked and ignored
   files, so a unit sees exactly what a fresh checkout of its commit holds.
   Before it starts each unit, the run reads holds that
   [`nyl hold`](/nyl/reference/cli/hold/) sent it, and starts no held unit;
4. resolves again at the same S, because new receipts unblock dependents, and
   repeats from step 3 until nothing is ready;
5. carries out lifecycle work: units that left the ownership set, and
   teardowns an operator requested, are torn down, retained, released, or
   dropped, consumers first ([Deletion and holds](/nyl/orchestration/deletion/));
6. writes one transition commit per state ref, then releases the lease.

A failed execution records a condition and keeps the unit's previous receipt.
An execution that may have had effects without completing, such as one that
passed its timeout, is uncertain. A retryable failure runs again at the next
reconcile; a non-retryable failure, and an uncertain unit whose kind does not
converge, wait for [`nyl recover`](/nyl/reference/cli/recover/).
[Recovery](/nyl/orchestration/recovery/) explains conditions, recovery
policies, and how a run takes over from a run that died, and the
[Command guide](/nyl/orchestration/resource-guides/command/#failures-and-retries)
describes how a Command converges. `--unit` executes only the named units
and does no lifecycle work; resolution still covers the whole environment,
and a named unit whose dependency has no current receipt stays blocked.
`--leaving` does only the lifecycle work.

A unit that fails, becomes uncertain, waits for an approval, or is blocked
does not stop the units that do not depend on it: a run reconciles everything
it can. With `--fail-fast`, the run starts no further unit after the first
failure, uncertain outcome, or approval wait; units already running finish,
and the run still checkpoints them and writes its transition commit. The
units it did not start are `blocked` with the reason `--fail-fast stopped the
run after <unit>`, and it warns with `NYL-RUN-STOPPED`.

## Transition commits

A reconcile that changes state writes one commit to each state ref it
changes, and a reconcile that changes nothing writes none. The message
starts with a headline and a machine-readable summary of every unit's
result, followed by trailers naming the run, the source commit, the state
commits it read, the runner, and the evaluation time:

```text
nyl: reconcile dev (1 imported, 1 uncertain)

units:
  database: {"executionKey":"sha256:6916707da65148797708b9f4754dbf1e9fe43eb229a2a6e5db09b933c3b6611f","result":"imported","run":"01a1732a-11d1-7ed6-a3cc-174dc69c7856"}
  migrate: {"reason":"run 01a1732a-11d1-7ed6-a3cc-174dc69c7856 was executing it when its lease was broken","result":"uncertain"}
recovered: [migrate]
imported: [database]
leaseBroken: {"at":"2026-10-25T13:05:12.386099409Z","by":"Alice Example <alice@example.com>","reason":"runner was killed"}

Nyl-Operation: reconcile
Nyl-Run-Id: 01a1732a-44de-7b8d-90ca-4602a61d3a51
Nyl-Environment: dev
Nyl-Source-Commit: 8400718ac2236aa6b71cd5a9c43d4ddd94545257
Nyl-Read-Desired: 1c139deeb2d48d5c13cbda789c5ad66a70b3c59d
Nyl-Read-Observed: 1c139deeb2d48d5c13cbda789c5ad66a70b3c59d
Nyl-Runner: unknown@localhost
Nyl-Evaluated-At: 2026-10-25T13:05:12Z
Nyl-Version: 0.5.9
```

Each unit's `result` is one of:

| Result | Meaning |
| --- | --- |
| `current` | The unit already had a current receipt. |
| `executed` | The unit ran, with this `executionKey`. |
| `imported` | A run that died or lost its lease checkpointed the unit with the same execution key, and that checkpoint, from `run`, became its receipt. |
| `recovered` | The unit was uncertain, and this run executed it again or found its effects applied. |
| `failed` | The execution failed, with its `category` and whether it is `retryable`. |
| `uncertain` | Effects may have happened; the unit waits for its recovery policy or `nyl recover`. |
| `blocked` | A reference or dependency has no current receipt, a held unit it depends on has none, or `--fail-fast` stopped the run first. |
| `awaiting-approval` | The unit needs an approval, such as `--approve` in a `--local` run, including a drifted unit that is not safe to re-run. |
| `drifted` | The unit's receipt is current but drifted, and this run did not repair it (`driftRepair: manual`, or not named by `--unit`). |
| `held` | The unit is [held](/nyl/orchestration/deletion/#holds); the detail names the source commit whose changes wait, and a pending teardown. |
| `pending-teardown` | The unit left the ownership set and its teardown has not started: it waits for `--allow-teardown`, its deleting dependents, readiness, or an approval. |
| `tearing-down` | The unit's teardown started and continues later, such as a publication waiting under its `teardownWait`. |
| `removed` | The unit was torn down, and its files were removed. |
| `retained` | The unit left the ownership set with `deletionPolicy: Retain` and became a tombstone. |
| `released` | The unit's resources were left in place, because its kind cannot tear down, and its state was removed or held. |

`held`, `drifted`, `pending-teardown`, and `tearing-down` make a reconcile
exit 2, because work remains; a held unit whose teardown request a
`nyl teardown` only recorded exits 0. `removed`, `retained`, and `released`
exit 0.

`imported` lists the units whose receipts came from checkpoints of earlier
runs, and `recovered` the units this run marked uncertain while taking over or
recovered by executing them again. `leaseBroken` appears when the run took
over a lease that [`nyl lease break`](/nyl/reference/cli/state/#nyl-lease-break)
broke. `driftRepaired` lists the units the run executed again because they
drifted, and `reapplied` records who forced which units with `--reapply`, and
why; each appears only when it is not empty:

```text
nyl: reconcile dev (1 executed, 1 awaiting-approval)

units:
  cache: {"result":"current"}
  config: {"executionKey":"sha256:42971c708f5916f2fab3a4319476c9a88123240b1cac2134e08d570e504a40b1","result":"executed"}
  extra: {"result":"current"}
  seed: {"result":"awaiting-approval"}
  worker: {"result":"current"}
recovered: []
imported: []
driftRepaired: [config]
```

Lifecycle work adds its details: `released` and `retained` list the units
and what they left in place, `allowDependents` the tombstones an override
passed, `confirmedRemoval` how a publication's teardown wait ended,
`incomplete` what a teardown may have left behind, `sourceOverride` a
teardown's `--source`, `teardownRequested` the held units whose teardown
request was recorded, and `held` the units a hold reached during the run.
The headline counts each result except `current`.

State commits are authored by `--author "Name <email>"`, else by Git's
`user.name` and `user.email`; without either, `nyl reconcile` exits 1 with
`NYL-COMMIT-IDENTITY-MISSING`. Commands that only read state, `plan`, `status`,
and `get`, need no identity.

## See also

- [Recovery](/nyl/orchestration/recovery/)
- [Environments and state](/nyl/orchestration/environments-and-state/)
- [Using Command](/nyl/orchestration/resource-guides/command/)
- [Environment](/nyl/reference/resources/gitops.nyl/v1/environment/)

## Approvals

A unit with `approval: manual` executes only with an invocation approval.
`--approve <unit>` authorizes its initial desired execution key; if references
resolve to a different key later in the run, the unit waits for a new approval.
An initially blocked unit cannot be approved for a key that does not yet exist.
A unit excluded by `--unit` cannot be approved.

For a kind that reports change digests, `approval: {mode: manual, bind: plan}`
accepts `--approve <unit>=sha256:…`. The driver recomputes the plan before
effects and waits with the new digest if it changed. The same rule covers
destroy plans. Without a digest, the approval is recorded as unreviewed;
`requireDigest: true` refuses it. `--approve-all` approves candidates without
reviewed digests, so those requiring a digest still wait.

Receipts and transition summaries record approvers, their source, the digest,
and whether it was reviewed. `--approved-by alice,bob --approval-source <url>`
records `identity: asserted`. A workstation run otherwise records the Git user
as `local`. In CI, a configured lookup or GitHub lookup records `verified`;
without a lookup or token Nyl records the CI run source with no approver. These
are audit identities: the CI gate controls who can authorize the invocation.

Configure other CI systems with an optional hook:

```toml
[approval]
lookup = ["./ci/approvers.sh"]
```

The hook runs in the project directory with `NYL_ENVIRONMENT`,
`NYL_APPROVAL_UNITS` (a JSON array of explicitly approved names), and
`NYL_APPROVAL_DIGESTS` (a JSON object mapping names to digests or null). It
prints `{"by": ["alice"], "source": "production gate", "url": "https://ci.example/run/1"}`.
Nyl records the command and result as `verified`. An invalid response, failure,
cancellation, or 30-second timeout stops the invocation before unit effects
with `NYL-APPROVAL-INVALID`. Explicit identity flags take precedence over lookups.

In GitHub Actions, `GITHUB_TOKEN` (or `GH_TOKEN`) with Actions read permission
allows Nyl to query the run review history. It records approved reviewers for
the GitHub environment whose name matches the Nyl environment. The token
remains in the lookup adapter and is never automatically admitted to unit tools.

## Multi-environment runs

`plan`, `reconcile`, `status`, `detect-drift`, and `attest` accept repeated `-e`, a label
selector with `-l`, or an EnvironmentGroup with `-g`:

```bash
nyl reconcile -e infra -e services
nyl plan -l stage=production -o json
nyl detect-drift -g production
nyl reconcile -e infra -e services --unit infra/network --unit services/api
```

Environments run in dependency order, producers first, with ties by name.
Each environment resolves its own source, takes its own lease when writing,
and writes its own transition commit. Every selected environment runs even
when an earlier one fails or waits. The invocation exits with the most severe
category across its environment report sections.

A cross-environment reference uses the producer's declared environment:

```yaml
values:
  networkId:
    fromUnit: {environment: infra, unit: network, output: id}
```

`dependsOn` also accepts `{environment: infra, unit: network}`. The producer
must select and enable the unit, and its receipt must be current. A run reads
each producer's state once, without its lease; provenance records that
producer's observed commit. A consumer run before its producer has a receipt
waits. In one invocation, consumers read the receipts that preceding producer
runs just wrote. References outside the selection read existing state.
Environment dependency cycles fail validation.

If a selected producer fails or waits, including when its lease is busy,
consumers wait instead of using its old receipts. Independent units still run.

Planning reads current receipts rather than preview outputs. If a preceding
producer plan changes a referenced unit, the consumer plan waits and names
that producer's planned change, even when `--unit` hides it from the producer's
report. A producer cannot be torn down while other
environments' stored desired units reference it unless `--allow-dependents`
permits that action.

In a selection containing several environments, unit options must name their
environment: `--unit services/api` and `--approve services/api=<digest>`.
Unqualified names and names outside the selection fail. `--allow-teardown`
and `--allow-dependents` apply to all selected environments; `--source` takes
exactly one environment. Deliberate actions such as `teardown`, `hold`,
`recover`, and `state` commands continue to take one environment.

Stored incarnations protect their resource ownership until removed, including
held units and units awaiting teardown whose source no longer declares them.
If an environment's state cannot be read, environments
that claim native resources are refused until access returns; independent
units without resource claims can still run.
