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
| Artifact kinds: `ContainerImage`, `PublishedTree` | `artifacts.gitops.nyl/v1` (see [Artifacts](#artifacts)) |
| Plugin unit and artifact kinds | The plugin's own groups, such as `units.acme.example/v1` (see [Plugin drivers](#plugin-drivers)) |

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
- A kind either publishes fixed results as artifacts (`OciImage`,
  `KubernetesPublication`; see [Artifacts](#artifacts)), in which case
  `outputs` is rejected, or lets the unit declare outputs (`Command`,
  `Terraform`, `OpenTofu`). Declared outputs use
  the Release input type set (`string`, `integer`, `number`, `boolean`,
  `object`, `array`), optional `description`, and optional `sensitive`. Only
  declared outputs are recorded. A `sensitive`
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
- The uid fences every receipt, checkpoint, and deletion. A name whose desired
  file is `deleting` cannot start a new incarnation until the deletion
  completes.
- Changing a unit's `apiVersion` or `kind` under the same name ends the old
  incarnation: it is deleted under its deletion policy, and the new kind
  starts a new incarnation. A unit never inherits another kind's receipts.
- An explicit teardown closes an incarnation even while the unit stays in the
  ownership set; the next incarnation receives a new uid (see
  [Deletion](#deletion)).

### References

| Reference | Resolves to | Freshness |
| --- | --- | --- |
| `fromUnit: {unit, output, pointer}` | A declared, non-sensitive output of a unit in the same environment; `pointer` optionally selects inside an `object` or `array` output | The producer's receipt must be current |
| `fromUnit: {unit, artifact, kind, pointer}` | A field of an artifact the unit published; `kind` optionally asserts the artifact kind | The producer's receipt must be current and list the artifact's digest |
| `fromPromotion: {path, value}` | A value in a PromotionRecord in this environment's desired state | The record must exist; broken selectors are errors |

- References are structured objects, never template lookups, so every
  dependency edge is visible in the rendered spec. Templating may compute a
  reference's fields; the edge is known once the spec is rendered.
- A reference to a unit outside the ownership set, to an undeclared or
  sensitive output, or one that closes a cycle, is a resolution error.
- The value is checked against the producer's declared output type or the
  artifact kind's schema and, where the consumer's schema declares one, the
  consumer field's type.

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
- It publishes a `PublishedTree` artifact named `tree` with the published
  commit and ownership-index digest (`published`). `mode: observe` additionally records Argo CD acceptance and
  health observations (M5), per the roadmap's health evidence section. Direct
  application through Nyl is not a mode in the initial scope.

## Artifacts

Kinds with fixed results publish artifacts instead of outputs. Artifacts are
their own kinds in `artifacts.gitops.nyl/v1`:

| Kind | Published by | `spec` |
| --- | --- | --- |
| `ContainerImage` | `OciImage` | `repository`, `digest`, `reference` (`<repository>@<digest>`), `platforms`, `tags` |
| `PublishedTree` | `KubernetesPublication` | `repository`, `branch`, `commit`, `indexDigest` |

```yaml
# observed/units/web-image/artifacts/image.yaml
apiVersion: artifacts.gitops.nyl/v1
kind: ContainerImage
metadata:
  name: image
  unit: {environment: dev, name: web-image, uid: 2b90…}
executionKey: sha256:…
spec:
  repository: registry.example.com/web
  digest: sha256:4f0c…
  reference: registry.example.com/web@sha256:4f0c…
  platforms: [linux/amd64, linux/arm64]
  tags: [nyl-1a2b3c4d5e6f7a8b, dev]
```

A reference reads an artifact through `fromUnit` with `artifact` instead of
`output`, an optional `kind` assertion, and a `pointer` into the artifact's
`spec`:

```yaml
# Release input on a DeploymentTarget
releaseInputs:
  platform/web:
    image:
      fromUnit: {unit: web-image, artifact: image, kind: ContainerImage, pointer: /reference}
---
# OpenTofu variables
spec:
  variables:
    web_image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
    web_platforms: {fromUnit: {unit: web-image, artifact: image, pointer: /platforms}}
```

- A `kind` that does not match the artifact is a resolution error.
- The receipt lists each artifact with the digest of its canonical form; the
  artifact file is checked against that digest before any value is read.
- PromotionPath selectors can read artifact fields the same way.
- Plugin drivers may define artifact kinds in their own groups.

## State layout

Desired and observed documents live under fixed top-level directories, so the
layout does not depend on whether the two refs coincide:

```text
state.yaml                                   # state location and history, written by `nyl state`
desired/
  environment.yaml                           # resolved Environment, source commit, ownership set
  units/<unit>.yaml                          # DesiredUnit, including lifecycle (deletion, hold)
  promotions/<path>.yaml                     # PromotionRecords
observed/
  units/<unit>.yaml                          # ObservedUnit: latest receipt and condition
  units/<unit>/artifacts/<name>.yaml         # artifacts of the latest receipt
  units/<unit>/observations/<type>.yaml      # latest observation per type (drift, health)
```

With separate refs, `desired/` exists only on the desired ref and `observed/`
only on the observed ref; `state.yaml` exists on both.

Every state file is YAML. State records use `apiVersion: gitops.nyl/v1` with
the kinds `StateRecord`, `EnvironmentRecord`, `DesiredUnit`, `PromotionRecord`,
`ObservedUnit`, `Observation`, `Lease`, and `Run`; artifacts use their own
kinds. JSON Schemas for all of them are generated from the Rust types and
published with the other resource references. Digests, including execution
keys, are computed over a canonical JSON form, so formatting never affects
them. A reader rejects a state file whose kind version it does not know.

Each unit has one desired file and at most one observed file, holding only the
latest state. Earlier states exist in Git history, which is enough for audit
and promotion: a consumer cites a producer's receipt by execution key and the
observed commit that holds it, and promotion walks the history of one file.

### Desired unit

```yaml
apiVersion: gitops.nyl/v1
kind: DesiredUnit
unit: {environment: dev, name: database, uid: 5d2a…, apiVersion: units.gitops.nyl/v1, kind: OpenTofu}
lifecycle:
  state: active                            # active | deleting | held
sourceCommit: 3e7b…
sourceRevision: 3e7b…                      # effective revision for repository content
spec:                                      # rendered, references still as expressions
  source: {path: infra/database}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}
  outputs: {host: {type: string}, port: {type: integer}}
  approval: {mode: manual, bind: plan}
resolvedSpec:                              # references replaced by values
  source: {path: infra/database}
  variables: {vpc_id: vpc-0abc123}
  outputs: {host: {type: string}, port: {type: integer}}
  approval: {mode: manual, bind: plan}
provenance:
  /variables/vpc_id: {unit: network, uid: 8c1f…, executionKey: sha256:…, observedCommit: 41f0…}
executionKey: sha256:…
readiness: {state: ready}
# while blocked:
# readiness:
#   state: blocked
#   reasons: [{pointer: /variables/vpc_id, reason: network has no current receipt}]
```

- `sourceRevision` is S unless the unit's `source` field names a repository or
  carries a promoted revision.
- `resolvedSpec` is what PromotionPath selectors such as `input: /source` or
  `input: /releases/platform~1web/image` point into.
- `provenance` records, per reference, the receipt or PromotionRecord the value
  came from.
- `executionKey` is a digest of the unit's `apiVersion`, `kind`, and driver
  behavior version, `resolvedSpec` without the common fields that do not affect
  what runs (`enabled`, `dependsOn`, `approval`, `deletionPolicy`, `timeout`),
  the selected source bytes, and any command-unit fingerprint output. It
  excludes the source commit ID, provenance, readiness, lifecycle, Nyl's
  release version, and external tool versions, so a new source commit with
  identical inputs, a producer re-execution that yields identical outputs, or a
  Nyl upgrade does not change it.
- A blocked unit keeps its rendered spec and source commit, so it resolves
  later from new evidence without rereading source.
- `lifecycle` carries deletion and holds (see [Deletion](#deletion)).

**Behavior versions.** Each driver declares a behavior version and bumps it
only when its execution semantics change in a way that requires re-execution.
`outputs` stays in the execution key, because a receipt must contain every
declared output. When a unit's execution key changed only because of its
driver's behavior version or its `outputs` declaration, and its driver's
recovery policy is not `converge`, the unit waits for `--approve` instead of
re-running automatically. External tool versions, such as Terraform's, are
recorded in receipts for audit; a unit that should re-run on a tool change pins
the version in its spec, such as `version: 1.9.5`.

### Observed unit

```yaml
apiVersion: gitops.nyl/v1
kind: ObservedUnit
unit: {environment: dev, name: database, uid: 5d2a…, apiVersion: units.gitops.nyl/v1, kind: OpenTofu}
receipt:
  executionKey: sha256:…
  behaviorVersion: 1
  run: 0b8f6c1e-…
  desiredCommit: 77aa…
  startedAt: 2026-09-24T10:00:00Z
  finishedAt: 2026-09-24T10:07:12Z
  versions: {nyl: 0.7.0, tofu: 1.9.5}
  approval: {by: alice, source: github-environment production https://github.com/acme/infra/actions/runs/1234, digest: sha256:9f2c…}
  provenance:
    /variables/vpc_id: {unit: network, uid: 8c1f…, executionKey: sha256:…, observedCommit: 41f0…}
  outputs: {host: db.dev.internal, port: 5432}   # declared, non-sensitive only
  artifacts: []                                   # e.g. [{name: image, kind: ContainerImage, digest: sha256:…}]
condition: null
# or, when an operator or retry is needed:
# condition: {state: uncertain, since: 2026-09-24T11:12:00Z, run: 0b8f…, reason: run lost its lease}
# condition: {state: failed, since: …, run: …, category: tool-error, retryable: false, message: "tofu apply exited 1"}
```

- A receipt is **current** when its unit uid and execution key equal those of
  the unit's current desired document. When the desired document changes, the
  receipt remains until the next execution replaces it; it documents what ran
  but satisfies no reference.
- `condition` records the latest failure or uncertainty and stays until the
  unit next succeeds or an operator runs `recover`. A condition does not remove
  the last receipt.

**Consumer freshness.** A reference is satisfied only by a current producer
receipt. When a producer's desired document changes, its old receipt stops
being current, and every consumer that resolved from it becomes `blocked` on
that producer, even if the consumer already has its own receipt. The
consumer's existing receipt still documents what ran; the consumer executes
again only after the producer's new receipt exists and re-resolution changes
the consumer's execution key.

### Transition commits

Human review of the state refs is a goal, so each operation writes at most one
commit per state ref: one reconcile, one promotion, one teardown, one verify.
The commit is the complete transition, and an operation that changes nothing
writes nothing. The per-unit progress of a run lives on a disposable run ref
instead (see [Runs and leases](#runs-and-leases)).

The commit message carries a summary that machines parse, followed by Git
trailers:

```text
nyl: reconcile dev (2 executed, 1 failed, 1 blocked)

units:
  network:    {result: current}
  database:   {result: executed, executionKey: sha256:…, approval: {by: alice, source: …, digest: sha256:9f2c…}}
  web-image:  {result: failed, category: tool-error, retryable: true}
  kubernetes: {result: blocked, reason: web-image has no current receipt}
recovered: []
imported: []

Nyl-Operation: reconcile
Nyl-Run-Id: 0b8f6c1e-…
Nyl-Environment: dev
Nyl-Source-Commit: 3e7b…
Nyl-Read-Desired: 77aa…
Nyl-Read-Observed: 41f0…
Nyl-Runner: https://ci.example.com/runs/1234
Nyl-Evaluated-At: 2026-09-24T10:07:30Z
Nyl-Version: 0.7.0
```

- `Nyl-Read-Desired` and `Nyl-Read-Observed` name the state commits the
  operation started from. `Nyl-Evaluated-At` is the time used for time-based
  decisions such as lease expiry.
- Operator actions add `Nyl-Requested-By` and `Nyl-Reason`; approvals are part
  of the summary and of each receipt.
- Operations: `reconcile`, `promote`, `teardown`, `resume`, `recover`,
  `verify`, `state-init`, `state-copy`.
- Replaying operations in order, from the read commits and evaluation times
  they name, reproduces every state decision.

## Execution

`nyl reconcile -e <env>`:

1. **Lease.** Take the environment's run lease (see
   [Runs and leases](#runs-and-leases)) and import results left behind by
   earlier runs.
2. **Resolve.** Render the environment's units at source commit S and resolve
   references against current receipts and PromotionRecords.
3. **Select.** A unit is ready when its desired document is `ready` and
   `active`, it has no current receipt, its provenance receipts are all still
   current, and it is not awaiting approval. A unit with an `uncertain`
   condition is selected only when its driver's recovery policy allows it:
   `converge` executes it again, and `inspect` runs `inspect` first (see
   [Recovery](#recovery)). Under `manual`, the unit waits for `recover`.
4. **Wave.** Execute ready units, independent units in parallel up to a
   concurrency limit, and checkpoint each result on the run ref.
5. **Re-resolve.** New receipts may unblock dependents. Resolve again at the
   same S.
6. **Repeat** from step 3 until nothing is ready.
7. **Commit.** Write one transition commit per state ref, release the lease,
   and delete the run ref.

A run never reads a source commit newer than S; later source changes are
picked up by the next run.

**Who writes desired state.** Desired state is derived deterministically from
source and observed evidence, so the reconcile runner writes it. Review happens
on source commits (the pull requests that change units and bindings) and on
promotions (`changeGate: pullRequest` opens a pull request against the desired
ref). Separate refs let that promotion review and different branch protection
apply to desired state without mixing with observed commits.

**Selection with `--unit`/`--units`:**

- Resolution always covers the whole environment.
- Only the named units execute. A dependency without a current receipt leaves
  the named unit `blocked`; dependencies and dependents are not executed.

**Transition commit conflicts.** When the final push loses the race, typically
to a promotion, Nyl fetches and rebases its commit if the winning commits
changed none of the paths it writes or read. Otherwise it keeps its results,
which are facts, resolves again against the new state, and commits that. When
desired and observed are separate refs, the observed commit is written first,
so evidence is never lost to a desired conflict.

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
  otherwise the execution ends without effects and the unit waits for a new
  approval. Plan files never leave the runner.
- `--approve` for a unit excluded by `--unit`/`--units` is an error.
- Teardown of a manual unit needs approval the same way; for `bind: plan` the
  digest is that of the destroy plan.

Recording. Each approval is recorded in the receipt and in the transition
commit's summary: who approved (`by`), where (`source`), and the digest.

- `--approved-by` and `--approval-source` set the identity and source
  explicitly. Without them, a local run records the Git user, and a CI run
  records the CI run URL.
- Approvals can be automated with a CI approval gate. With GitHub environment
  protection rules, a `plan` job runs `nyl plan --output json` and passes the
  digest as a job output; an apply job with `environment: production` runs
  `nyl reconcile --approve database=<digest>` only after GitHub's reviewers
  approve. When a GitHub token with read access to Actions is available, Nyl
  reads the run's approvers from GitHub's run-approvals API and records them as
  the approval's `by`.

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

### Runs and leases

Coordination and progress live outside the desired and observed refs:

| Ref | Content | Lifetime |
| --- | --- | --- |
| `nyl/<env>/lease` | One `Lease`: run ID, runner, operation, deadline, units executing | Created at run start, deleted at run end |
| `nyl/<env>/runs/<run-id>` | The run's `Run` record and a checkpoint commit per finished unit: its receipt or failure, and its artifacts | Deleted after its results reach a transition commit |

1. **Lease.** A run creates the lease ref with compare-and-swap. If a lease
   exists and has not expired, the run exits 2 and reports who holds it. Only
   one state-writing operation runs per environment at a time.
2. **Deadline.** The deadline is the latest finish time of the units executing,
   by their `timeout`, plus a grace period that also absorbs clock skew. The
   runner updates the lease when it starts a unit; there are no heartbeats.
3. **Checkpoints.** Each finished unit is checkpointed on the run ref before
   the next wave. A crash loses no evidence.
4. **Takeover.** A run that finds an expired lease replaces it with
   compare-and-swap. It imports every leftover run ref: checkpointed results
   whose unit uid and execution key still match become receipts, the others
   are reported as superseded. Units the dead run was executing without a
   checkpoint get an `uncertain` condition. All of this lands in the new run's
   transition commit, under `imported` and `recovered`.
5. **Lost lease.** A runner whose lease update fails stops starting units,
   checkpoints the units still running to its own run ref, and exits 4. The
   next run imports those results as above.

`nyl status` reads the lease and run refs, so it shows a run in progress and
the units it is executing.

### Recovery

An uncertain condition means effects may or may not have happened. Each driver
declares a recovery policy for execution and for teardown:

| Policy | Meaning | Initial drivers |
| --- | --- | --- |
| `converge` | Re-executing the same document is safe; the next `reconcile` executes it again | Terraform, OpenTofu, OciImage, KubernetesPublication, command with `idempotent: true` |
| `inspect` | The driver's `inspect` reports applied (with outputs), not applied, or unknown; applied records the receipt, not applied executes again, unknown falls back to `manual` | Drivers that can observe their effects |
| `manual` | Blocked until an operator runs `recover --retry` | Command units without `idempotent: true` |

`nyl recover -e <env> --unit <u> --retry [--reason <text>]` clears an uncertain
condition, or a non-retryable failure, and records the operator's decision and
reason, such as "verified nothing was applied", in its transition commit. It
does not execute by itself: the unit becomes ready and runs in the next
`reconcile`, so an operator can decide locally while CI executes. Accepting an
uncertain execution as applied requires outputs, so it is possible only through
`inspect`.

A retryable failure is retried by the next `reconcile`; a non-retryable one
waits for `recover --retry`.

## Deletion

Deletion and holds are recorded in the desired unit's `lifecycle` block, so a
unit's whole history is one file's diff and the last desired spec needed for
teardown is already there:

```yaml
lifecycle:
  state: deleting                    # active | deleting | held
  deletion:
    reason: omission                 # omission | teardown | replace | kind-change
    intent: teardown                 # retain | teardown
    requested: {by: alice, at: 2026-09-24T12:00:00Z, reason: decommission}
  # hold: {since: 2026-09-24T12:05:00Z, by: alice, reason: "incident 4711"}
```

- **Omission.** A unit that leaves the ownership set (removed from source, for
  example with `nyl delete unit <name>`, deselected, or `enabled: false`) keeps
  its desired file with `state: deleting`, `reason: omission`, and the intent
  from its `deletionPolicy`. Units still referencing it make resolution fail,
  so a unit and its consumers are removed together.
- **Retain** (default). The run removes the desired and observed files. Native
  resources are untouched and history keeps everything. The transition
  commit's summary lists the retained unit, so an unintended omission, such as
  a selector typo, is visible without having destroyed anything.
- **Teardown.** The driver's teardown runs from the desired file's spec, with
  the driver's teardown recovery policy. Among deleting units, units are torn
  down before the units they depended on. Teardown caused by omission requires
  `--allow-teardown` on `reconcile`; without it the unit is
  `pending-teardown`. A driver without teardown support leaves the unit
  visibly blocked. When teardown succeeds, the run removes both files.
- **Explicit teardown.** `nyl teardown -e <env> --unit <u>` tears a unit down
  whether or not it is in the ownership set. The command is itself explicit
  intent, so it needs no `--allow-teardown`.
  - For a unit outside the ownership set (a deleting unit, or a retained unit
    whose last desired file is recovered from history), it completes the
    deletion.
  - For a unit still in the ownership set, it replaces the unit
    (`reason: replace`): the incarnation is torn down, and the next
    `reconcile` writes a new desired file with a new uid. Dependents are
    blocked until the new receipt exists, then run with its outputs. This
    rebuilds corrupted resources or rotates something that can only be
    recreated.
  - With `--hold`, the incarnation is torn down as above and the desired file
    stays with `state: held` and a `hold` block, so `reconcile` does not create
    the next incarnation; its dependents stay blocked. `nyl resume -e <env>
    --unit <u>` removes the hold; the next `reconcile` then writes a new uid.
- **Holds.** A hold is for incidents: it stops a broken unit from being
  recreated or re-run while it is investigated, without a source change and
  its review cycle. It suits units that can be recreated without losing data;
  tearing down a database destroys its data. A hold is the only desired state
  that does not come from source, and it is an explicit, recorded operator
  action. Long-term removal belongs in source, through `enabled: false` or
  removing the unit.
- **Source removal and teardown.** `nyl delete unit <name>` edits source like
  the other `nyl delete` resources: it removes the declaration after checking
  that the remaining project is valid, which rejects removing a unit other
  units still reference. The removal takes effect in every environment that
  selected the unit, at the next `reconcile`, under its deletion policy. `nyl
  teardown` acts on state directly and never edits source.
- **Fencing.** A new incarnation cannot take a name whose desired file is
  `deleting`; it waits until the file is removed, then receives a new uid. A
  kind change under the same name is a deletion with `reason: kind-change`.

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
  files after the execution.
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
- Local runs write transition commits only to those refs, with a
  `Nyl-Local: true` trailer, keep their lease and run refs local too, and never
  push. `nyl status -e <env> --local` shows the local view.
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
| Lease grace period | 10 minutes | `nyl.toml` |
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
    fn teardown(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Outcome>>; // lifecycle: deleting
}

enum Supported<T> {
    Supported(T),
    Unsupported,
}

enum Outcome {
    Succeeded { outputs: Outputs, artifacts: Vec<Artifact> }, // typed artifact documents
    Failed { category: FailureCategory, retryable: bool },
    Uncertain,
}
```

- `ExecutionContext` provides the worktree at the effective source revision,
  the resolved spec, admitted environment variables and secrets, the deadline,
  a cancellation signal, the unit's observed file, and a transcript sink that
  masks secret values.
- Drivers run external tools only through the context, so transcripts,
  masking, and deadlines are uniform.
- Methods for capabilities a driver lacks return `Supported::Unsupported`, which is
  reported, never silently skipped.
- Every type crossing the trait (context, desired unit, observed unit,
  artifact, outcome, plan report, inspection, drift) is plain data with a JSON
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
  capabilities, recovery policies, and any artifact kinds it publishes. Nyl
  treats every unit kind a registered plugin describes as a unit. `units.gitops.nyl/v1` is reserved for built-in
  drivers.
- The plugin implements `plan`, `reconcile`, `verify`, `inspect`, and
  `teardown` over the same serialized types. Nyl keeps resolution, leases,
  checkpoints, receipts, transition commits, secret admission, transcript masking, and deadlines.
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
  fails the execution. Failures are not retryable unless `idempotent: true`.
- `verify` exits 0 for clean, 2 for drift, and anything else for an error. It
  records a drift observation, never a receipt.
- There is no sandbox beyond the working directory and the environment; the
  command runs with the runner's permissions. Isolation such as containers is
  an M7 decision.
- Secrets require a configured secrets provider; the project must provide one
  before command units can use `env.secrets`.

## Commands

| Command | Effect | Writes |
| --- | --- | --- |
| `plan` | Resolve and run driver planning for ready units; report blocked units and incomplete plans | Nothing |
| `reconcile` | Resolve, then execute waves until nothing is ready | One transition commit per state ref |
| `status` | Report each unit's state from one snapshot of both refs | Nothing |
| `verify` | Run driver verification against current receipts | One observed commit with the latest observations |
| `recover` | Clear an uncertain condition or non-retryable failure for re-execution | One observed commit |
| `teardown` | Tear down a unit, replacing it if still selected; `--hold` keeps it down | One transition commit per state ref |
| `resume` | Remove a hold | One desired commit |
| `state init` | Create state at the configured location; `--fresh` starts over deliberately | `state.yaml` on each ref |
| `state copy` | Copy state history from another location | Copied history plus `state.yaml` |
| `promote` | See the roadmap's promotion section | PromotionRecord |

All are top-level `nyl` commands. `release` is taken by Kubernetes release
history and `delete` by source editing, so removing a hold is `resume` and
tearing down is `teardown`.

### Inspection

`nyl get` inspects both declarations and state. Without `-e`, it reads source;
with `-e <env>`, it reads that environment's state.

```bash
nyl get units                      # declarations: kind, labels, environments selecting each
nyl get units -e dev               # state: joins desired and observed
NAME        KIND                   STATE     RECEIPT   LAST RUN
network     OpenTofu               current   current   0b8f… 10:07
database    OpenTofu               blocked   stale     0b8f… 10:07   network has no current receipt
web-image   OciImage               failed    current   0b8f… 10:05   tool-error (retryable)
kubernetes  KubernetesPublication  held      —         —

nyl get unit database -e dev -o yaml             # exact DesiredUnit and ObservedUnit documents
nyl get unit database -e dev -o yaml --observed  # only one of them (--desired, --observed)
nyl get unit database -e dev --pointer /receipt/outputs/host

nyl get artifacts -e dev
UNIT        NAME   KIND            SUMMARY
web-image   image  ContainerImage  registry.example.com/web@sha256:4f0c…
kubernetes  tree   PublishedTree   deploy/dev@9c1e…

nyl get artifact web-image/image -e dev -o yaml
nyl get artifact web-image/image -e dev --pointer /spec/reference

nyl get environments
nyl get promotion-paths
nyl get promotions -e staging      # PromotionRecords with each value's source and evidence
```

- Tables join desired and observed state for reading. `-o yaml` and `-o json`
  return the exact persisted documents, never a synthesized merge.
- `--pointer` prints one value selected by a JSON Pointer, for scripts. Reading
  an artifact verifies its digest against the receipt first.
- `--revision <commit>` reads state as of an earlier state commit; `--local`
  reads a local run's state. Output formats: `table` (default), `wide`,
  `yaml`, `json`, `name`.
- `get` only reads: it needs read access to the state repository, never takes
  the lease, and exits 0 whenever it can read the requested state.
- `nyl create` and `nyl delete` stay source-only, for environments, units, and
  promotion paths. Artifacts and state records have no `create` or `delete`:
  drivers produce them, and state changes only through the operations above.
- `nyl status -e <env>` remains the environment overview: the run holding the
  lease and what it is executing, blockers, and suggested next actions. It
  keeps the exit categories below, so CI can check whether everything is
  reconciled.

Common options: `-e`/`--environment`, `--unit`/`--units`,
`--approve <unit>[=<digest>]`, `--approved-by`, `--approval-source`,
`--allow-teardown`, `--local`, `--concurrency`, and `--output json` for versioned machine results on
stdout, with human diagnostics on stderr.

Each unit has one state in `status`: `blocked` (with the unresolved reference or
non-current dependency), `ready`, `awaiting-approval`, `running` (with the run
holding the lease), `uncertain`, `failed`, `current`, `held`,
`pending-teardown`, or `tearing-down`. Whether the latest receipt is current is
shown as a separate attribute.

Exit categories, where "selected" means the units the invocation was allowed to
execute (all of them without `--unit`/`--units`):

| Exit | Meaning |
| --- | --- |
| 0 | Every selected unit is current, or its deletion completed |
| 1 | Configuration, resolution, or operational error |
| 2 | Not everything was reconciled, and nothing failed: a unit is held, blocked by a held dependency, or waiting for evidence, approval, `--allow-teardown`, or another run's lease |
| 3 | At least one execution failed |
| 4 | At least one execution is uncertain and needs recovery, or the run lost its lease |

The highest applicable category wins. Holds are reported as exit 2 on purpose:
a run that leaves units unreconciled says so. A pipeline that expects a hold
names the units it should reconcile with `--unit`/`--units`, and then exits 0
when those are current.

## Walkthroughs

**Successful dependency wave.** `network` has no references and runs in wave 1;
its receipt is checkpointed on the run ref. Re-resolution makes `database`
ready; it runs in wave 2 with `vpcId` in its resolved spec. `kubernetes`
depends on `database` and on `web-image`'s `ContainerImage` through its
target's bindings and runs in wave 3. The run ends with one desired and one
observed commit whose summary lists all three. Exit 0.

**Unavailable upstream output.** `web-image` fails in wave 1 with a retryable
failure. `kubernetes` stays `blocked` on `web-image` with its rendered spec
retained; `database` is unaffected. The transition commit records the failure
as `web-image`'s condition. Exit 3. The next run retries `web-image`; once its
receipt exists, `kubernetes` resolves without rereading source.

**Effects without a receipt.** A runner applies OpenTofu for `database` and dies
before checkpointing. Its lease expires; the next run takes it over, imports
the dead run's checkpoints, and gives `database` an `uncertain` condition.
OpenTofu's policy is `converge`, so the same run plans the same desired
document again, finds no changes, and records the receipt. Its transition
commit lists `database` under `recovered`. Nothing is assumed from the missing
checkpoint.

**Competing runners.** Two CI jobs reconcile `dev` at once. One creates the
lease; the other finds it held, reports the holder, and exits 2. If the holder
is lost, its lease expires, and a later run takes over as above.

**Promotion with stale source evidence.** Dev's `web-image` has a new desired
document without a receipt for its execution key. Promoting from `dev` at
`published` walks the history of the selected units' observed files and takes
the newest desired revision whose selected units each had a receipt for their
execution key; the new, unexecuted document is skipped and the previous one is
promoted. At `healthy`, the value comes from the publication the consuming
Application runs. If no revision meets the required level, promotion blocks
and reports which unit lacks evidence.

**Deletion.** `cache` and its only consumer `worker` are removed from source;
`cache` has `deletionPolicy: Teardown`, `worker` the default `Retain`. The next
run marks both `deleting`, removes `worker`'s files, and reports `cache` as
`pending-teardown`, exiting 2. A run with `--allow-teardown` tears `cache` down
and removes its files. A new `cache` added in the meantime waits for that and
then receives a new uid.

## Remaining questions

| Question | Needed by |
| --- | --- |
| Approver lookup for CI systems other than GitHub | M3 |

Driver-specific questions are in the [infrastructure units contract](infrastructure-units.md).
