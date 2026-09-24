# Orchestration core

**Status:** draft M1 contract for M3–M5. See [ROADMAP.md](../ROADMAP.md) and the
[Release inputs contract](release-inputs.md).

This contract defines environments, units, desired and observed state,
execution, recovery, deletion, the driver interface, the command unit, and the
effects of the orchestration commands. Promotion rules are in the roadmap's
promotion and health evidence sections; this contract defines the state they
read.

Nothing here changes existing commands. A project without Environments and
units never touches orchestration state.

## Resources

| Resources | API group |
| --- | --- |
| Environment, PromotionPath (and the existing GitRepository) | `gitops.nyl/v1` |
| Built-in unit kinds: `Command`, `Terraform`, `OciImage`, `KubernetesPublication` | `units.gitops.nyl/v1` |
| Plugin unit kinds | The plugin's own group, such as `units.acme.example/v1` (see [Plugin drivers](#plugin-drivers)) |

Every resource in a unit group is a unit; the group identifies the family and
the kind selects the driver, as `components.k8s.nyl/v1` does for component
invocations. Discovery follows Git visibility across the project, like the
rendered GitOps resources. `nyl create`, `nyl get`, and `nyl delete` gain
`environment`, `unit`, and `promotion-path` resources.

### Environment

```yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata:
  name: production
spec:
  unitSelector:
    matchLabels: {tier: platform}
  values:
    stateKey: production/database
    target: production
  state:
    repositoryRef: {name: platform-state}   # or inline `repository`
    desiredRef: nyl/production/desired      # default nyl/<environment>/desired
    observedRef: nyl/production/observed    # default nyl/<environment>/observed
  protectedRefs: [main, 'release/*']        # default: the source repository's default branch
```

- An Environment is static at discovery, like a DeploymentTarget.
- `unitSelector` matches literal unit `metadata.labels` across all unit kinds;
  an empty selector selects every unit. The selected, enabled units are the
  environment's authoritative ownership set: leaving it is how deletion is
  requested (see [Deletion](#deletion)).
- `values` is exposed to unit templates as `values`, and the sanitized
  Environment as `environment`. Repository credentials are never exposed.
- `state` follows the shape of DeploymentTarget `publication`: a GitRepository
  reference (`repositoryRef`) or inline `repository`, reading through its
  `repoURL` and writing through its `publishURL`. Without either, state uses
  the source checkout's `origin` remote; CI should name a GitRepository.
- `desiredRef` and `observedRef` may name the same ref; the layout is identical
  either way (see [State layout](#state-layout)).
- `protectedRefs` lists the refs that pinned commits must be reachable from
  (see [Pinned commits](#pinned-commits)).

### Units

```yaml
apiVersion: units.gitops.nyl/v1
kind: Terraform
metadata:
  name: database
  labels: {tier: platform}
spec:
  source: {path: infra/database}
  backend: {key: '{{ values.stateKey }}'}
  variables:
    vpcId: {fromUnit: {unit: network, output: vpcId}}
  outputs:
    host: {type: string}
    port: {type: integer}
  enabled: true
  dependsOn: []
  approval: auto
  deletionPolicy: Retain
  timeout: 30m
```

- The envelope (`apiVersion`, `kind`, `metadata.name`, `metadata.labels`) is
  literal, so selection happens before rendering. `spec` is a Nyl template
  rendered once per selecting Environment.
- Every unit kind shares the common fields `enabled` (evaluated after
  rendering; `false` leaves the ownership set), `outputs`, `dependsOn`,
  `approval` (see [Approval](#approval)), `deletionPolicy` (`Retain` or
  `Teardown`), `timeout`, and `env` (see [Credentials](#credentials)). They are one Rust struct flattened into each kind's spec, so
  their schema and documentation are identical everywhere. The remaining fields
  belong to the kind, and each kind has its own schema and generated reference
  page.
- References may appear in any field the kind's schema marks as accepting
  them.
- A kind either defines its outputs itself (`OciImage`,
  `KubernetesPublication`), in which case `outputs` is rejected, or lets the
  unit declare them (`Command`, `Terraform`, `OpenTofu`). Declared outputs use
  the Release input type set (`string`, `integer`, `number`, `boolean`,
  `object`, `array`), optional `description`, and optional `sensitive`. Only
  kind-defined or declared outputs are recorded. A `sensitive`
  output is validated but never persisted and cannot be referenced; secrets
  move through the secrets provider, not through outputs. Drivers reject
  native-tool sensitive outputs that are not declared sensitive.
- `dependsOn` lists units that must have a current receipt first, for ordering
  without a data reference. References add dependencies implicitly.
- Repository content from elsewhere uses the ApplicationGroup source shape in
  the kind's `source` field: `repositoryRef` or `repository`, a human
  `revision`, a locked `commit`, and `path`. `nyl update source-locks` refreshes
  these locks with the others. Without a repository, `source.path` is read at
  the source commit.

### Identity

- Unit names are unique per environment across all unit kinds, so references
  name only the unit. A unit's address is `<environment>/<unit>`.
- Each incarnation has a `uid`, a UUIDv7 generated when resolution first
  publishes its desired document. Two resolvers racing to create the same unit
  are serialized by compare-and-swap; the loser re-reads and adopts the
  published uid.
- The uid fences every attempt, receipt, and tombstone. A name held by a
  tombstone cannot start a new incarnation until the tombstone closes.
- Changing a unit's `apiVersion` or `kind` under the same name ends the old
  incarnation: it is tombstoned under its deletion policy, and the new kind
  starts a new incarnation. A unit never inherits another kind's receipts.
- An explicit teardown closes an incarnation even while the unit stays in the
  ownership set; the next incarnation receives a new uid (see
  [Deletion](#deletion)).

### References

| Reference | Resolves to | Freshness |
| --- | --- | --- |
| `fromUnit: {unit, output, pointer}` | A declared, non-sensitive output of a unit in the same environment; `pointer` optionally selects inside an `object` or `array` output | The producer's receipt must be current |
| `fromPromotion: {path, value}` | A value in a PromotionRecord in this environment's desired state | The record must exist; broken selectors are errors |

- References are structured objects, never template lookups, so every
  dependency edge is visible in the rendered spec. Templating may compute a
  reference's fields; the edge is known once the spec is rendered.
- A reference to a unit outside the ownership set, to an undeclared or
  sensitive output, or one that closes a cycle, is a resolution error.
- The value is checked against the producer's declared output type and, where
  the consumer's schema declares one, the consumer field's type.

### Kubernetes publication unit

```yaml
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata:
  name: kubernetes
  labels: {tier: platform}
spec:
  target: '{{ values.target }}'
  mode: publish        # publish | observe
```

- The unit places its DeploymentTarget into the environment. A target
  referenced by publication units in two environments is an error; a target
  referenced by none rejects `fromUnit` and `fromPromotion` bindings.
- Its dependencies are the units named by the target's `fromUnit` bindings and
  the PromotionRecords named by its `fromPromotion` bindings.
- Its resolved spec contains every Release input of its target, keyed as
  `releases.<group>/<release>.<input>`: `value`, `fromFile` at the source
  commit, locked `fromGit`, `fromUnit`, `fromPromotion`, and `fromPublication`.
- `fromPublication` is resolved like every other binding. Resolution reads the
  state file from the publication branch head B, or the carried file from the
  runner's working tree. The value enters the resolved spec and the execution
  key; B is recorded only as provenance. A new state file therefore changes
  the execution key and triggers an ordinary execution, while unrelated commits
  on a shared publication branch, including Nyl's own publications, change
  nothing.
- At execution, `publish-tree` builds on the current branch head. If the head
  has moved past B but the state file is unchanged, it proceeds on the new
  head; if the state file changed, the execution ends without publishing and
  the unit is resolved again in the next wave.
- Its receipt records the published commit and ownership-index digest
  (`published`). `mode: observe` additionally records Argo CD acceptance and
  health observations (M5), per the roadmap's health evidence section. Direct
  application through Nyl is not a mode in the initial scope.

## State layout

Desired and observed documents live under fixed top-level directories, so the
layout does not depend on whether the two refs coincide:

```text
state.yaml                             # state location and history, written by `nyl state`
desired/
  environment.yaml                     # resolved Environment, source commit, ownership set
  units/<unit>.yaml                    # desired unit documents
  tombstones/<unit>.yaml               # deletion intent, uid-fenced
  promotions/<path>.yaml               # PromotionRecords
  holds/<unit>.yaml                    # operator holds after explicit teardown
observed/
  units/<unit>/receipt.yaml            # latest receipt, or absent
  units/<unit>/artifacts/<name>.yaml   # artifacts of the latest receipt
  units/<unit>/attempt.yaml            # open or most recent attempt
  observations/<unit>/<timestamp>.yaml
```

With separate refs, `desired/` exists only on the desired ref and `observed/`
only on the observed ref; `state.yaml` exists on both.

Every state file is YAML and carries `apiVersion: gitops.nyl/v1` and a kind
(`StateRecord`, `EnvironmentRecord`, `DesiredUnit`, `Tombstone`,
`PromotionRecord`, `Hold`, `Receipt`, `Artifact`, `Attempt`, `Observation`).
Their JSON Schemas are generated from the Rust types and published with the
other resource references. Digests, including execution keys, are computed
over a canonical JSON form, so formatting never affects them. A reader rejects
a state file whose kind version it does not know.

Each unit has one receipt file: the latest receipt, or none after teardown or
retention. Earlier receipts
exist only in Git history, which is enough for audit and promotion: a consumer
cites a producer's receipt by execution key and the observed commit that holds
it, and promotion walks the history of one receipt path.

### Desired unit

A desired unit is the execution snapshot of one incarnation:

- identity: environment, unit name, uid, `apiVersion` and `kind`;
- the source commit S it was rendered from, and the effective source revision
  for units that execute repository content: S, unless the unit's `source`
  field names a repository or carries a promoted revision;
- the rendered spec, with references still written as expressions;
- `resolvedSpec`: the rendered spec with each reference replaced by its value.
  PromotionPath selectors such as `input: /source` or
  `input: /releases/platform~1web/image` are JSON Pointers into this document;
- `provenance`: for each reference, the producer unit, uid, and execution key
  of the receipt it came from, or the PromotionRecord and value;
- `executionKey`: a digest of the unit's `apiVersion`, `kind`, and driver
  behavior version, `resolvedSpec` without the common fields that do not affect
  what runs (`enabled`, `dependsOn`, `approval`, `deletionPolicy`, `timeout`),
  the selected source bytes, and any command-unit fingerprint output. It excludes the source commit
  ID, provenance, readiness, Nyl's release version, and external tool
  versions, so a new source commit with identical inputs, a producer
  re-execution that yields identical outputs, or a Nyl upgrade does not change
  it;
- `readiness`: `ready`, or `blocked` with each unresolved reference and why.

A blocked unit keeps its rendered spec and source commit, so it resolves later
from new evidence without rereading source.

**Behavior versions.** Each driver declares a behavior version and bumps it
only when its execution semantics change in a way that requires re-execution.
`outputs` stays in the execution key, because a receipt must contain every
declared output. When a unit's execution key changed only because of its
driver's behavior version or its `outputs` declaration, and its driver's
recovery policy is not `converge`, the unit waits for `--approve` instead of
re-running automatically. External tool versions, such as
Terraform's, are recorded in receipts for audit; a unit that should re-run on a
tool change pins the version in its spec, such as `terraform: {version: 1.9.5}`.

### Receipt

A receipt records one successful execution:

- subject: environment, unit, uid; the execution key and driver behavior
  version;
- the desired commit that held the executed document;
- attempt ID, Nyl version, external tool versions, start and finish times;
- `provenance` as resolved at execution time;
- `outputs` (declared, non-sensitive) and artifact descriptors with digests.

A receipt is **current** when its uid and execution key equal those of the
unit's current desired document. When the desired document changes, the
existing receipt remains until the next execution replaces it; it documents
what ran but satisfies no reference.

**Consumer freshness.** A reference is satisfied only by a current producer
receipt. When a producer's desired document changes, its old receipt stops
being current, and every consumer that resolved from it becomes `blocked` on
that producer, even if the consumer already has its own receipt. The
consumer's existing receipt still documents what ran; the consumer executes
again only after the producer's new receipt exists and re-resolution changes
the consumer's execution key.

### Commit metadata

Every commit Nyl writes to a state ref records exactly one event. Its message
carries Git trailers that machines parse with `git interpret-trailers`:

```text
nyl: dev/database receipt

Nyl-Event: receipt
Nyl-Operation: reconcile
Nyl-Operation-Id: 0b8f6c1e-…
Nyl-Environment: dev
Nyl-Unit: database
Nyl-Unit-Uid: 5d2a…
Nyl-Execution-Key: sha256:…
Nyl-Attempt-Id: 91c4…
Nyl-Source-Commit: 3e7b…
Nyl-Read-Desired: 77aa…
Nyl-Read-Observed: 41f0…
Nyl-Runner: https://ci.example.com/runs/1234
Nyl-Version: 0.7.0
```

`Nyl-Read-Desired` and `Nyl-Read-Observed` name the state commits the decision
was based on. Approvals, recovery decisions, teardowns, and holds add
`Nyl-Approved-By` or `Nyl-Requested-By` with the operator identity the runner
was given, and `Nyl-Reason` when one was supplied.

| Event | Ref | Changes |
| --- | --- | --- |
| `desired-updated` | desired | `desired/environment.yaml`, `desired/units/*` |
| `tombstone` | desired | `desired/tombstones/<unit>.yaml`, unit document removed |
| `promotion` | desired | `desired/promotions/<path>.yaml` |
| `attempt-started` | observed | `attempt.yaml` opened (execution or teardown) |
| `receipt` | observed | receipt and artifacts added, attempt closed |
| `attempt-failed` | observed | attempt closed with category and retryability |
| `attempt-uncertain` | observed | attempt marked uncertain after its deadline |
| `attempt-superseded` | observed | a late runner records its result after replacement |
| `recovery` | observed | an operator clears an uncertain or failed attempt for re-execution |
| `hold` | desired | `desired/holds/<unit>.yaml` added after an explicit teardown with `--hold` |
| `hold-released` | desired | hold removed |
| `retained` | observed | receipts and attempt moved out of `observed/units/<unit>/` |
| `teardown` | observed | teardown recorded, unit files removed |
| `tombstone-closed` | desired | tombstone removed after `retained` or `teardown` |
| `observation` | observed | `observed/observations/…` |
| `state-initialized` | both | `state.yaml` created, or reset with `--fresh` |
| `state-copied` | both | `state.yaml` records the copy source |

Invariants:

- A commit changes only the paths that belong to its event and subject. An
  auditor can check this from the trailers and the diff.
- `Nyl-Operation-Id` links every commit of one invocation.
- Replaying events in order, using the read commits they name, reproduces every
  state decision without relying on commit order in time. Time-based decisions
  record the time they used: `attempt-uncertain` and `attempt-superseded`
  carry `Nyl-Evaluated-At` alongside the attempt's deadline, so a replay can
  verify that the deadline had passed.

## Execution

`nyl reconcile -e <env>`:

1. **Resolve.** Render the environment's units at source commit S, resolve
   references against current receipts and PromotionRecords, and publish
   `desired-updated` and `tombstone` commits for what changed.
2. **Select.** A unit is ready when its desired document is `ready`, it has no
   current receipt, its provenance receipts are all still current, it has no
   open attempt, it is not held, and it is not awaiting approval. An uncertain
   attempt does not block selection when the driver's recovery policy allows
   it: `converge` starts a new attempt directly, and `inspect` runs `inspect`
   first (see [Recovery](#recovery)). Under `manual`, the unit waits for
   `recover --retry`.
3. **Wave.** Execute ready units, independent units in parallel up to a
   concurrency limit (see [Attempts](#attempts)).
4. **Re-resolve.** New receipts may unblock dependents. Resolve again at the
   same S and publish what changed.
5. **Repeat** from step 2 until nothing is ready, then report.

A run never reads a source commit newer than S; later source changes are
picked up by the next run.

**Who writes desired state.** Desired state is derived deterministically from
source and observed evidence, so the reconcile runner writes it. Review happens
on source commits (the pull requests that change units and bindings) and on
promotions (`changeGate: pullRequest` opens a pull request against the desired
ref). Separate refs let that promotion review and different branch protection
apply to desired state without competing with the frequent observed commits.

**Selection with `--unit`/`--units`:**

- Resolution always covers the whole environment.
- Only the named units execute. A dependency without a current receipt leaves
  the named unit `blocked`; dependencies and dependents are not executed.

### Approval

```yaml
spec:
  approval: auto                       # default
  # or
  approval: {mode: manual, bind: plan} # bind: desired | plan
```

- A manual unit executes only when the invocation approves it. `approval:
  manual` is short for `{mode: manual, bind: desired}`.
- `bind: desired` approves one execution of the current desired document:
  `--approve database`.
- `bind: plan` approves exactly the changes a reviewed plan showed. It requires
  a kind whose `plan` reports a change digest (Terraform and OpenTofu do).
  `nyl plan` prints each manual unit's digest, and `--approve
  database=sha256:…` authorizes that digest. At execution the driver plans
  again in the same run and applies that plan only if its digest matches;
  otherwise the attempt ends without effects and the unit waits for a new
  approval. Plan files never leave the runner.
- `--approve` for a unit excluded by `--unit`/`--units` is an error.
- Teardown of a manual unit needs approval the same way; for `bind: plan` the
  digest is that of the destroy plan.

Recording. The `attempt-started` commit records every approval:

```text
Nyl-Approved-By: alice
Nyl-Approval-Source: github-environment production https://github.com/acme/infra/actions/runs/1234
Nyl-Approval-Digest: sha256:9f2c…
```

- `--approved-by` and `--approval-source` set the identity and source
  explicitly. Without them, a local run records the Git user, and a CI run
  records the CI run URL.
- Approvals can be automated with a CI approval gate. With GitHub environment
  protection rules, a `plan` job runs `nyl plan --output json` and passes the
  digest as a job output; an apply job with `environment: production` runs
  `nyl reconcile --approve database=<digest>` only after GitHub's reviewers
  approve. When a GitHub token with read access to Actions is available, Nyl
  reads the run's approvers from GitHub's run-approvals API and records them as
  `Nyl-Approved-By`.

```yaml
# .github/workflows/production.yaml (excerpt)
jobs:
  plan:
    outputs: {digest: ${{ steps.plan.outputs.digest }}}
    steps:
      - id: plan
        run: echo "digest=$(nyl plan -e production --unit database --output json | jq -r '.units.database.changeDigest')" >> "$GITHUB_OUTPUT"
  apply:
    needs: plan
    environment: production        # required reviewers approve here
    steps:
      - run: nyl reconcile -e production --unit database --approve "database=${{ needs.plan.outputs.digest }}"
```

**Compare-and-swap.** Each event has a read set: the paths it read and the
commits named in `Nyl-Read-Desired` and `Nyl-Read-Observed`. When a push loses
the race, Nyl fetches, and reapplies its commit only if the winning commits
changed none of its write paths and none of its read-set paths on that ref.
Otherwise it re-evaluates from step 2. When desired and observed are separate
refs, a desired change during execution cannot be detected by the observed
push; it does not need to be, because the resulting receipt carries the
execution key and is simply not current if the desired document moved on.

### Attempts

An attempt is the claim, the lease, and the recovery record for one execution
or teardown:

```yaml
apiVersion: gitops.nyl/v1
kind: Attempt
id: 91c4…
operation: reconcile
unit: {environment: dev, name: database, uid: 5d2a…}
executionKey: sha256:…
runner: https://ci.example.com/runs/1234
startedAt: 2026-09-24T10:00:00Z
deadline: 2026-09-24T10:35:00Z
state: running
```

1. **Claim.** The runner writes `attempt-started`. The winning commit holds the
   unit until the deadline: start plus `timeout` plus a grace period. The
   grace period also absorbs clock skew between runners. A runner that sees an
   open attempt skips the unit and reports who holds it.
2. **Run.** The driver executes with the deadline and a cancellation signal.
   Native locks, such as Terraform state locking, remain the second guard.
3. **Record.** Success writes `receipt`, closing the attempt. A definite
   failure writes `attempt-failed`, with the driver's retryability. A
   retryable failure is retried by the next `reconcile`; a non-retryable one
   waits for `recover --retry`.
4. **Expire.** The next `reconcile` marks an attempt past its deadline
   `attempt-uncertain`. `status` reports such an attempt as expired without
   writing anything.
5. **Late results.** A runner finishing after its attempt was marked uncertain
   may still write its receipt as long as no other attempt has started; the
   receipt closes the uncertain attempt. Once a new attempt has started, the
   late runner writes `attempt-superseded` with its result instead.

No heartbeats are written; executions that can exceed their timeout must raise
`timeout`.

### Recovery

An uncertain attempt means effects may or may not have happened. Each driver
declares a recovery policy for execution and for teardown:

| Policy | Meaning | Initial drivers |
| --- | --- | --- |
| `converge` | Re-executing the same document is safe; the next `reconcile` retries automatically | Terraform, image build, Kubernetes publication, command with `idempotent: true` |
| `inspect` | The driver's `inspect` reports applied (with outputs), not applied, or unknown; applied writes the receipt, not applied retries, unknown falls back to `manual` | Drivers that can observe their effects |
| `manual` | Blocked until an operator runs `recover --retry` | Command units without `idempotent: true` |

`nyl recover -e <env> --unit <u> --retry [--reason <text>]` clears an uncertain attempt, or a failed non-retryable one, and
records the operator's decision and reason, such as "verified nothing was
applied". It does not execute by itself: the unit becomes ready and runs in the
current or next `reconcile`, so an operator can decide locally while CI
executes. Accepting an uncertain attempt as applied requires outputs, so it is
possible only through `inspect`.

## Deletion

- **Omission.** A unit that leaves the ownership set (removed from source, for
  example with `nyl delete unit <name>`, deselected, or `enabled: false`) gets a `tombstone` carrying its uid, its last
  desired document, and its deletion policy. Units still referencing it make
  resolution fail, so a unit and its consumers are removed together.
- **Retain** (default). The runner writes `retained`, moving the unit's
  receipts out of `observed/`, then `tombstone-closed`. Native resources are
  untouched; history keeps everything. Status lists recently retained units,
  so an unintended omission, such as a selector typo, is visible without
  having destroyed anything.
- **Teardown.** The driver's teardown runs from the tombstone's desired
  document, as an attempt with the driver's teardown recovery policy. Among
  tombstones, units are torn down before the units they depended on, using the
  dependency graph recorded in their last desired documents. Teardown caused
  by omission requires `--allow-teardown` on `reconcile`; without it the unit
  is `pending-teardown`. A driver without teardown support leaves the
  tombstone visibly blocked. Success writes `teardown`, then
  `tombstone-closed`.
- **Explicit teardown.** `nyl teardown -e <env> --unit <u>` tears a unit down whether or not it is in the ownership set. The command
  is itself explicit intent, so it needs no `--allow-teardown`.
  - For a unit outside the ownership set (a pending tombstone, or a retained
    unit whose last desired document is recovered from history), it completes
    the deletion.
  - For a unit still in the ownership set, it replaces the unit: the current
    incarnation is torn down and closed, and the next `reconcile` creates a new
    incarnation with a new uid. Dependents are blocked until the new receipt
    exists, then run with its outputs. This rebuilds corrupted resources or
    rotates something that can only be recreated.
  - With `--hold`, the incarnation is tombstoned and torn down as above, and a
    hold is recorded in the desired ref as well, so `reconcile` does not create
    the next incarnation. `status` shows the unit as `held`, and its dependents
    stay blocked. `nyl resume -e <env> --unit <u>` removes the hold; the next
    `reconcile` then creates a new incarnation with a new uid. A hold is the
    only desired state that does not come from source; it is an explicit,
    recorded operator event. Long-term removal still belongs
    in source, through `enabled: false` or removing the unit.
- **Source removal and teardown.** `nyl delete unit <name>` edits source like
  the other `nyl delete` resources: it removes the declaration after checking
  that the remaining project is valid, which rejects removing a unit other
  units still reference. The removal takes effect in every environment that
  selected the unit, at the next `reconcile`, under its deletion policy. `nyl
  teardown` acts on state directly and never edits source.
- **Fencing.** A new unit with a tombstoned name stays blocked until the
  tombstone closes, then receives a new uid. Teardown attempts carry the old
  uid.

## Credentials

Every driver starts its tools from an empty environment plus what the unit
admits, so nothing leaks by default and a plugin driver follows the same rule:

```yaml
spec:
  env:
    passthrough: [AWS_REGION, AWS_ROLE_ARN, AWS_WEB_IDENTITY_TOKEN_FILE]
    secrets:
      TF_VAR_db_password: database-password   # key in the project's secrets provider
```

- `env.passthrough` copies named variables from the runner environment;
  `env.secrets` reads keys from the project's secrets provider. A string secret
  is passed as is; an object or array secret is passed as compact JSON.
- Kinds add fixed variables they always need, such as `PATH` and `HOME`, and
  document them.
- A kind may offer typed helpers for well-known credentials, such as
  `registryAuth` for `OciImage`. Nyl turns a helper into environment variables
  or temporary files, validates it against the kind's schema, and removes the
  files after the attempt.
- Secret values and everything a helper derives from them are masked in
  transcripts and never recorded.
- The execution key covers the names in `env` and helper structure, never
  values: rotating a secret does not re-run a unit.

## State lifecycle

### Initialization and moves

`state.yaml` records where the state lives (repository and refs) and every
initialization, copy, or fresh start, each as its own event.

- The first run for an environment requires `nyl state init -e <env>`, which
  writes `state.yaml` at the configured location. `reconcile` never creates
  state implicitly.
- When an Environment's `state` points to a location without `state.yaml`,
  `reconcile` refuses and names both ways forward:
  - `nyl state copy -e <env> --from <repository> [--desired-ref …]
    [--observed-ref …]` pushes the existing refs' history to the new location
    and records a `state-copied` event. Units keep their uids and receipts.
  - `nyl state init -e <env> --fresh` starts over deliberately. Every unit
    becomes a new incarnation and executes again: Terraform and OpenTofu
    converge against their existing backends, images are rebuilt, and
    non-idempotent commands run again. The command prints what that means and
    records a `state-initialized` event marked fresh.
- A location whose `state.yaml` names a different environment is an error, so
  two environments can never share state by accident.

### Local runs

`--local` runs orchestration against local-only state, for developing units and
trying changes against real evidence before CI:

- The first local run for an environment copies the remote state refs to local
  refs `refs/nyl/local/<env>/desired` and `refs/nyl/local/<env>/observed`; later
  local runs continue from them. `--local --reset` copies again.
- Local runs write state commits only to those refs, with a `Nyl-Local: true`
  trailer, and never push. `nyl status -e <env> --local` shows the local view.
- Effects are real, and local state is never pushed. CI's next run executes
  those units again. That is safe for `converge` drivers; a local run
  therefore executes units with other recovery policies only when they are
  named with `--approve`, and warns that CI will run them again.

### Pinned commits

Every commit Nyl pins must stay fetchable: locked unit sources, promoted source
revisions, and locked `fromGit` bindings.

- A pinned commit must be reachable from one of the Environment's
  `protectedRefs` in its repository. The default is that repository's default
  branch. Resolution checks this when a lock or promotion enters desired
  state; `nyl promote` checks it against the target environment.
- An unreachable commit is a resolution error that names the commit and the
  refs searched. A commit from a squash-merged branch is therefore rejected
  before anything depends on it.
- The source commit S of a run itself may be any commit, so a dev environment
  can reconcile from a feature branch; its revisions are checked only when
  they are promoted.

### Credentials and branch protection

- State pushes use the same Git credentials as `publish-tree`: an SSH key, an
  SSH agent, or an HTTPS token, through the GitRepository's `publishURL`.
- Nyl never force-pushes a state ref. A non-fast-forward state ref is treated
  as corruption and stops every command until an operator repairs it.
- Recommended protection: only the runner identity may push to the observed
  ref; the desired ref accepts pushes from the runner and promotion pull
  requests with required reviews; force pushes and deletion are disabled on
  both.

### Defaults

| Setting | Default | Configured in |
| --- | --- | --- |
| Concurrency per wave | 4 | `nyl.toml`, `--concurrency` |
| Attempt grace period | 10 minutes | `nyl.toml` |
| `timeout` | Command 30m, Terraform/OpenTofu 60m, OciImage 60m, KubernetesPublication 10m | unit `timeout` |

## Drivers

Built-in drivers are Rust implementations behind one trait:

```rust
trait Driver {
    fn api_version(&self) -> &'static str; // units.gitops.nyl/v1 for built-ins
    fn kind(&self) -> &'static str;
    fn behavior_version(&self) -> u32;
    fn capabilities(&self) -> Capabilities; // plan, reconcile, verify, teardown, inspect, observe
    fn recovery(&self) -> RecoveryPolicies; // for execution and teardown
    fn spec_schema(&self) -> Schema;        // kind fields; common fields are added by Nyl

    fn plan(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<PlanReport>>;
    fn reconcile(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Outcome>>;
    fn verify(&self, ctx: &ExecutionContext, unit: &DesiredUnit, receipt: &Receipt) -> Result<Supported<Drift>>;
    fn inspect(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Inspection>>;
    fn teardown(&self, ctx: &ExecutionContext, tombstone: &Tombstone) -> Result<Supported<Outcome>>;
}

enum Supported<T> {
    Supported(T),
    Unsupported,
}

enum Outcome {
    Succeeded { outputs: Outputs, artifacts: Vec<ArtifactDescriptor> },
    Failed { category: FailureCategory, retryable: bool },
    Uncertain,
}
```

- `ExecutionContext` provides the worktree at the effective source revision,
  resolved inputs, admitted environment variables and secrets, the deadline, a
  cancellation signal, previous receipts, and a transcript sink that masks
  secret values.
- Drivers run external tools only through the context, so transcripts,
  masking, and deadlines are uniform.
- Methods for capabilities a driver lacks return `Supported::Unsupported`, which is
  reported, never silently skipped.
- Every type crossing the trait (context, desired unit, receipt, tombstone,
  outcome, plan report, inspection, drift) is plain data with a JSON
  representation. Handles such as the cancellation signal and transcript sink
  stay on Nyl's side and have message equivalents. This keeps a process adapter
  possible without changing the model.

### Plugin drivers

Plugins are not part of the initial scope; this is the direction the serializable
boundary keeps open, to be decided in M7 once command units have shown which
drivers are needed.

- A plugin is an executable, such as `nyl-driver-helmfile`, declared in
  `nyl.toml` with a pinned version and digest. It speaks a versioned JSON
  protocol over stdin and stdout.
- Its `describe` call returns its unit kinds in a group it owns, such as
  `units.acme.example/v1` `Helmfile`, with their schemas, behavior versions,
  capabilities, and recovery policies. Nyl treats every kind a registered
  plugin describes as a unit. `units.gitops.nyl/v1` is reserved for built-in
  drivers.
- The plugin implements `plan`, `reconcile`, `verify`, `inspect`, and
  `teardown` over the same serialized types. Nyl keeps resolution, attempts,
  receipts, state commits, secret admission, transcript masking, and deadlines.
- A plugin runs with the runner's permissions, like a command unit; pinning by
  digest is the supply-chain control.
- The command unit is the protocol's step zero: one command, without a schema
  or lifecycle verbs.

## Command unit

```yaml
apiVersion: units.gitops.nyl/v1
kind: Command
metadata:
  name: seed
spec:
  files: ['scripts/seed/**']
  values:
    bucket: {fromUnit: {unit: storage, output: bucket}}
  command: ['./scripts/seed/run.sh']
  verify: ['./scripts/seed/verify.sh']
  fingerprint: ['./scripts/seed/tool-versions.sh']
  env:
    passthrough: [AWS_REGION, AWS_PROFILE]
    secrets:
      DB_PASSWORD: database-password      # key in the project's secrets provider
  idempotent: true
  outputs:
    seedVersion: {type: string}
  timeout: 10m
```

- The command runs in a worktree at the effective source revision, starting
  from an empty environment plus:
  - `NYL_INPUTS`: path to a JSON file with the resolved `values`;
  - `NYL_OUTPUTS`: path where the command writes one JSON object of outputs;
  - `NYL_UNIT` and `NYL_ENVIRONMENT`;
  - variables and secrets admitted through the common `env` field (see
    [Credentials](#credentials)).
- Following the general rule, the execution key covers `command`, `verify`,
  `fingerprint`, `idempotent`, `outputs`, the files matched by `files`,
  resolved `values`, the names in
  `env.passthrough` and `env.secrets` but not their values, and the output of
  `fingerprint`. Changing a passthrough variable or rotating a secret does not
  re-run the command.
- `fingerprint` lets the unit contribute to its own execution key, for example
  by printing the versions of the tools it uses. It runs during resolution in
  the same worktree with the passthrough variables but no secrets, must be
  fast and deterministic, and must exit 0. Its stdout is digested into the
  execution key and stored in the desired document, truncated to 4 KiB, for
  audit. A change in its output re-runs the unit like any other input change.
- Files outside `files` may be read but do not trigger re-execution.
- Stdout and stderr form the transcript and are never parsed. Secret values
  and `sensitive` outputs are masked in it.
- Exit code 0 with a valid outputs file is success. Any other exit code is a
  failure; a missing or invalid outputs file, or an undeclared output, also
  fails the attempt. Failures are not retryable unless `idempotent: true`.
- `verify` exits 0 for clean, 2 for drift, and anything else for an error. It
  records an observation, never a receipt.
- There is no sandbox beyond the working directory and the environment; the
  command runs with the runner's permissions. Isolation such as containers is
  an M7 decision.
- Secrets require a configured secrets provider; the project must provide one
  before command units can use `env.secrets`.

## Commands

| Command | Effect | Writes |
| --- | --- | --- |
| `plan` | Resolve and run driver planning for ready units; report blocked units and incomplete plans | Nothing |
| `reconcile` | Resolve, then execute waves until nothing is ready | Desired updates, tombstones, attempts, receipts, retention, teardowns |
| `status` | Report each unit's state from one snapshot of both refs | Nothing |
| `verify` | Run driver verification against current receipts | Observations |
| `recover` | Clear one uncertain or non-retryable attempt for re-execution | `recovery` |
| `teardown` | Tear down a unit, replacing it if still selected; `--hold` keeps it down | Tombstone (plus a hold with `--hold`), then teardown events |
| `resume` | Remove a hold | `hold-released` |
| `state init` | Create state at the configured location; `--fresh` starts over deliberately | `state-initialized` |
| `state copy` | Copy state history from another location | `state-copied` |
| `promote` | See the roadmap's promotion section | PromotionRecord |

All are top-level `nyl` commands. `release` is taken by Kubernetes release
history and `delete` by source editing, so removing a hold is `resume` and
tearing down is `teardown`. `nyl get environments`, `nyl get units`, and `nyl
get promotion-paths` list declarations; `nyl status` shows state.

Common options: `-e`/`--environment`, `--unit`/`--units`,
`--approve <unit>[=<digest>]`, `--approved-by`, `--approval-source`,
`--allow-teardown`, `--local`, `--concurrency`, and `--output json` for versioned machine results on
stdout, with human diagnostics on stderr.

Each unit has one state in `status`: `blocked` (with the unresolved reference or
non-current dependency), `ready`, `awaiting-approval`, `running` (with the
holding attempt), `expired`, `uncertain`, `failed`, `current`, `held`,
`retained`, `pending-teardown`, or `tearing-down`. Whether the latest receipt is current is
shown as a separate attribute.

Exit categories, where "selected" means the units the invocation was allowed to
execute (all of them without `--unit`/`--units`):

| Exit | Meaning |
| --- | --- |
| 0 | Every selected unit is current, retained, or torn down |
| 1 | Configuration, resolution, or operational error |
| 2 | Not everything was reconciled, and nothing failed: a unit is held, blocked by a held dependency, or waiting for evidence, approval, `--allow-teardown`, or another runner |
| 3 | At least one attempt failed |
| 4 | At least one attempt is uncertain and needs recovery |

The highest applicable category wins. Holds are reported as exit 2 on purpose:
a run that leaves units unreconciled says so. A pipeline that expects a hold
names the units it should reconcile with `--unit`/`--units`, and then exits 0
when those are current.

## Walkthroughs

**Successful dependency wave.** `network` has no references and runs in wave 1;
its receipt records `vpcId`. Re-resolution makes `database` ready; it runs in
wave 2 with `vpcId` in its resolved spec. `kubernetes` depends on `database`
and `web-image` through its target's bindings and runs in wave 3. Every step is
one commit with trailers sharing one operation ID. Exit 0.

**Unavailable upstream output.** `web-image` fails in wave 1 with a retryable
failure. `kubernetes` stays `blocked` on `web-image` with its rendered spec
retained; `database` is unaffected. Exit 3. The next run retries `web-image`;
once its receipt exists, `kubernetes` resolves without rereading source.

**Effects without a receipt.** A runner applies Terraform for `database` and
dies before writing its receipt. The attempt stays open until its deadline;
the next `reconcile` marks it uncertain. Terraform's policy is `converge`, so
the same run re-plans the same desired document, finds no changes, and writes
the receipt. Nothing is assumed from the missing receipt.

**Competing runners.** Two CI jobs reconcile `dev` at once. Both try to claim
`database`; one `attempt-started` commit wins, and the other job reports the
unit as held and moves on. If the holder is lost, its attempt expires, and a
later run recovers it.

**Promotion with stale source evidence.** Dev's `web-image` has a new desired
document without a receipt for its execution key. Promoting from `dev` at
`published` walks the history of each selected unit's `receipt.yaml` and takes
the newest desired revision whose selected units each had a receipt for their
execution key; the new, unexecuted document is skipped and the previous one is
promoted. At
`healthy`, the value comes from the publication the consuming Application
runs. If no revision meets the required level, promotion blocks and reports
which unit lacks evidence.

**Deletion.** `cache` and its only consumer `worker` are removed from source;
`cache` has `deletionPolicy: Teardown`, `worker` the default `Retain`.
Resolution writes both tombstones. `worker` is retained immediately. `cache` is
`pending-teardown` and `reconcile` exits 2 until run with `--allow-teardown`,
which tears it down and closes its tombstone. A new `cache` added in the
meantime waits for that and then receives a new uid.

## Remaining questions

| Question | Needed by |
| --- | --- |
| Approver lookup for CI systems other than GitHub | M3 |

Driver-specific questions are in the [infrastructure units contract](infrastructure-units.md).
