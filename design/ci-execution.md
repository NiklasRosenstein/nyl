# CI execution adapters

**Status:** planned direction. M3 provides a backend-neutral graph and reports;
M8 adds provider-neutral CI execution support and a first GitLab adapter.
Commands below are proposed interfaces, not current CLI capabilities.
See [ROADMAP.md](../ROADMAP.md), the
[orchestration core](orchestration-core.md), and the
[implementation architecture](implementation-architecture.md).

## Purpose and boundary

Expose each named Nyl unit as a separate CI job, with its own logs, duration,
status, and retry control, without authoring a second dependency graph.
Nyl owns identity, resolution, freshness, approvals, admission, and lifecycle.
CI owns event triggers, runner provisioning, credentials, and job presentation.
An adapter exports existing dependencies; it adds no workflow conditionals,
loops, matrices, or alternative readiness rules to unit declarations.

Unit identity remains the literal `metadata.name`, addressed as
`<environment>/<unit>`, independent of its source path. Job identifiers use a
deterministic, collision-checked encoding of that address, owned by the
orchestration layer so every adapter shares it. Reports retain the original
address. Provider-specific images, tags, and job setup belong to a separate
adapter configuration, pinned with the source, not to unit identity or the
execution key. That configuration cannot override dependencies, commands,
approvals, or Nyl's exit categories. Secrets never enter generated YAML.

Every job runs an ordinary Nyl command that an operator can also run from a
workstation. CI adds presentation and scheduling, never a capability the CLI
lacks, so a broken pipeline never blocks an operator (see
[Break-glass and repair](#break-glass-and-repair)).

## Provider-neutral execution contract

The execution graph and manifest are versioned Nyl documents. They carry unit
addresses, dependency edges, operation, the admitted source and configuration
bindings, and report locations. They contain no GitLab job IDs, `needs`, stage
names, resource groups, or CI environment-variable names. A provider adapter
maps this model to its scheduler and maps job observations back to the common
report model. Provider pipeline/job identifiers are opaque diagnostic metadata,
not unit identities, deployment generations, or evidence of successful effects.

A provider advertises capabilities such as generated workflows, dependency
scheduling, always-run aggregation, and pipeline serialization. A pure
orchestration function turns the graph and those capabilities into an export
plan: it rejects an unsupported requirement or selects a documented safe
fallback, and never silently omits jobs or weakens the execution contract. For
example, a provider without dynamic workflow loading can use a checked-in
generated workflow or a dispatcher that submits jobs, while each worker still
validates the same manifest. The adapter only serializes that plan and maps
runner profiles and events from its configuration. The first implementation
targets GitLab; no GitHub Actions or other provider support is claimed until its
adapter passes conformance tests.

A provider-neutral fixture must export through both the GitLab adapter and an
in-memory test adapter. They must retain identical unit addresses, dependencies,
source bindings, admission requirements, and semantic results. YAML layout and
provider display statuses may differ. Adding a provider must not require new
fields on unit kinds or a provider-specific state/receipt format.

## First backend: separate reconciliation invocations

The first GitLab backend creates one job per selected unit. Each job runs an
ordinary `reconcile --unit` at the admitted source. It is not a distributed
Nyl run: every invocation acquires the environment lease and records its own
transition. Independent jobs may be queued together, but effects within one
environment serialize. No per-unit leases are introduced.

The exporter reads the full environment ownership set, including units that
already have current receipts. It derives edges from `dependsOn` and `fromUnit`
references after structural rendering, including dependencies whose values are
not yet available. It does not require a complete native plan. Workers resolve
again against current evidence; a producer may execute and leave the consumer's
execution key unchanged. Such a consumer job reports a no-op.

- A job executes only its named unit. `--unit` does not expand dependencies.
  All producers selected in this environment receive jobs and ordering edges,
  even if their receipts were current at export time.
- Cross-environment references read current external receipts under the core
  contract. The first exporter supports one environment and does not implicitly
  schedule or mutate producer environments; unavailable evidence blocks the job.
- Filtering by changed files must never shrink the ownership set. Omission is
  deletion intent, not an optimization. The initial exporter has no unit filter.
- Generated graphs bind source repository, full source commit, project location,
  environment, operation, adapter configuration digest, and compatible Nyl
  protocol version. Workers take their source from the admission, never from a
  `--source` argument, and a checkout that does not match the admitted binding
  fails before resolution.
- Source is fixed across jobs; observed state is not. Each job rechecks receipts,
  ownership, holds, and approval digests. GitLab `needs` success is scheduling
  evidence only, never authority to execute with stale inputs.
- Pipeline serialization is an optional scheduling optimization, not the fence.
  The core environment lease serializes effects regardless of provider,
  including attempts retried outside the parent job's lifetime. This backend
  promises neither a shared observed snapshot nor one atomic transition across
  jobs.

### Leaving units

A reconciliation export that finds units leaving the ownership set, or already
`pending-teardown`, adds one lifecycle job after every unit job. It runs
`nyl reconcile -e <env> --leaving`, which selects exactly those units and
applies the core [deletion](orchestration-core.md#deletion) rules: `Retain`
units become tombstones, and teardown follows the reverse dependency order.
Without `--allow-teardown`, omission teardown leaves units `pending-teardown`
and the job exits 2, so a selector typo destroys nothing and the pipeline shows
the pending removal. Adapter configuration may render a manual variant passing
`--allow-teardown` in protected pipelines; otherwise an operator runs the same
command locally. Unit jobs never perform lifecycle work, and the read-only
summary job never reconciles anything.

Planning export adds the same lifecycle node as a read-only plan report listing
leaving units, reasons, and consequences, even if no units remain selected. It
writes no desired/observed state. Preserve `plan --fail-on-leaving` as an
explicit caller policy; removal does not fail planning by default. The
aggregate retains this report even when a unit plan is blocked or skipped.

## Deployment admission

Admission answers one question for workers: which deployment should this
environment run now. It protects against an old pipeline's retried or delayed
job redeploying an older source after a newer deployment, and, in a hardened
setup, against worker credentials deciding what is deployed. It is opt-in per
environment with `admission: required`; with the default `off`, unit jobs carry
only their source binding and the environment behaves exactly as in M3, so
nothing guards a retried old job beyond receipts and the lease.
`nyl ci export` warns with `NYL-CI-ADMISSION-OFF` for such environments.

### The admission record

A `DeploymentAdmission` holds the current generation: a generation ID unique
across state identities, a monotonic sequence, the source commit S and how the
environment's source rules chose it, the semantic graph digest, and the
normalized authorizing actor, request, and reason. It is stored as
`admission.yaml` on the environment's admission ref, `state.admissionRef`,
which defaults to the desired ref. Only a newer admission replaces it.

`nyl ci admit -e <env>` creates or confirms a generation under the environment
lease:

1. Resolve S with the environment's ordinary source rules (entry worktree,
   `revision`, `commit`, or `fromPromotion`), with the same reachability
   checks as any run. Admission never chooses a source those rules would not.
2. Compare with the current generation. The same S and graph digest return the
   current generation unchanged, so a retried dispatch is idempotent. A newer S
   (a descendant of the admitted commit, or for `fromPromotion` a newer or
   equal promotion sequence) mints the next generation.
3. Anything else is a rollback: an older or diverging commit, or an explicit
   `--source <rev>`. It requires `--rollback --reason <text>` and authorization
   under the dispatcher's policy, and is recorded as an override. Without it,
   admission refuses with `NYL-CI-ADMISSION-ROLLBACK`. A retried dispatch of an
   overtaken pipeline is therefore refused, not renewed.

Ancestry and promotion sequence are policy inputs for step 2, not authority:
only the record on the admission ref admits. Provider-specific identity checks
run in the dispatcher; the core accepts normalized authorization evidence
through the existing trust model. Graph extraction/export is read-only; the
semantic digest excludes admission IDs, provider job URLs, and presentation
metadata, avoiding a circular digest.

### Workers

A worker runs `nyl reconcile -e <env> --unit <u> --generation <id>`. After
acquiring the environment lease, before resolution writes, checkpoint import,
or driver calls, the core compares that generation with the current record:

- **Current:** the run proceeds at the admitted S with the normal rules.
  Approvals, holds, receipts, and recovery conditions are rechecked
  independently; admission neither grants plan approval nor permits teardown.
- **Superseded:** the run executes nothing, writes no desired/observed state,
  and reports the unit `superseded` with the warning
  `NYL-CI-ADMISSION-SUPERSEDED`, exit 0. The newer generation's own pipeline
  runs the unit; an overtaken pipeline shows skipped work, not failures.

Workers cannot admit themselves or obtain a new generation through retry. The
check runs inside the core gate under the same lease as the state mutation,
never as a shell preflight before a separate `reconcile`. Lease
acquisition/release may still change coordination refs.

Without `--generation`, a run in an `admission: required` environment works at
the current generation's S, so `nyl reconcile -e <env>` from a workstation
converges on what CI was deploying; before the first admission it exits 2 with
`NYL-CI-ADMISSION-MISSING`. `reconcile --source` is refused in such an
environment, because a different source is a rollback admission; read-only
`plan --source` previews stay available.

### What admission does not touch

Holds, resumes, recovery, attestations, verification, lifecycle work, and
direct local runs change unit state, not deployment intent, so they leave the
admission unchanged; workers recheck that unit state under the lease anyway.
`nyl state restore` rewrites desired and observed content and carries
`admission.yaml` forward unchanged, so a restore never moves the generation
back. `nyl state move` moves the admission with the state. A fresh
`nyl state init` starts without an admission, and its generation IDs never
match an earlier identity's, so older workers report superseded.

### Security profiles

The same commands and semantics serve every profile; only refs and credentials
differ.

| Profile | Setup | Guarantee |
| --- | --- | --- |
| Simple | One state ref; one CI credential that can push state and coordination refs; admission off or on | Leases, receipts, and, with admission on, the stale-retry fence. Admission is consistency, not a security boundary: any job could also push the admission ref |
| Separated | `state.admissionRef` on a branch only the dispatcher identity may push; workers push desired, observed, and coordination refs; plan jobs get read-only state | Worker and unit-authored code cannot mint, revive, or redirect deployments; they can only reconcile what was admitted |
| Strict | Separated, plus per-environment worker credentials, the dispatcher in a protected pipeline, and protected source and state branches | Least privilege per environment and role |

Dispatcher policy, including who may roll back, comes from protected
configuration, never from proposed unit source.

### Proposed GitLab flow

```sh
# Read-only graph export at the pinned source checkout:
nyl ci export gitlab -e config --operation reconcile --output pipeline.yml

# Trusted dispatch admits the graph and binds the generation into the jobs:
nyl ci admit -e config --graph pipeline.manifest.json --request "$CI_PIPELINE_ID"

# A generated unit job:
nyl reconcile -e config --unit kalix-talos --generation <id> --output json

# The generated lifecycle job, after every unit job:
nyl reconcile -e config --leaving --generation <id> --output json
```

The parent uploads `pipeline.yml` and the public graph manifest, then starts a
dynamic child pipeline using the artifact. The parent holds the resource group
`nyl-<env>` with `strategy: mirror`, so pipelines of one environment run one at
a time; unit jobs use the separate resource group `nyl-<env>-units`, so they
queue in GitLab instead of occupying runners while waiting for the lease.
`--wait-lease` then only absorbs runs outside CI; its value comes from adapter
configuration. Generated includes use literal paths. The adapter renders
validated argument vectors and YAML rather than interpolating names or authored
text into shell fragments. Runner setup admits credentials through the normal
driver/secret contracts; GitLab artifacts and dotenv files do not transport
authoritative unit outputs.

Each worker stores the typed operation report as an artifact even on failure;
diagnostics and driver transcripts remain in that unit's job log. Reports carry
unit address, source commit, generation, Nyl run ID, and the CI pipeline/job
URL. A summary job runs even after failures, includes skipped, superseded, or
missing jobs, and links to their reports. GitLab artifact retention is for
observability; Nyl state and receipts remain authoritative and survive artifact
expiry.

| Nyl result | CI behavior |
| --- | --- |
| Current, executed, or superseded (0) | Success, identifying no-op, execution, or superseded generation |
| Configuration/operational error (1) | Fail with the core diagnostic |
| Waiting, blocked, pending teardown, or lease wait exhausted (2) | Non-success with blocker and next action; never silently allowed to fail |
| Driver failure (3) | Fail, retaining the unit report |
| Uncertain or lease lost (4) | Fail with recovery guidance; no unconditional automatic retry |

GitLab's generic retry policy must not blindly repeat effects. A manual job retry
starts a fresh ordinary invocation with the same generation. The core checks
admission before receipts and its recovery policy: a superseded job skips;
within the current generation successful work becomes a no-op and an uncertain
non-idempotent command still requires recovery. After `nyl recover --retry`,
retrying the job is the intended next step, because recovery leaves the
generation current. CI manual buttons do not grant approval: the adapter
supplies evidence through the normal approval contract and Nyl revalidates its
binding at execution time.

For merge requests, export `--operation plan`. Jobs invoke only read-only
`plan --unit`; the adapter never reconciles a producer to complete a plan.
Planning needs no admission. First-time downstream plans can be incomplete
because no upstream output exists. Report exit 2 and its blockers honestly; a
successful upstream plan does not create a receipt or provide executable
preview values. Plan jobs use read-only state access and the driver's required
planning credentials, not reconciliation credentials or production state
writes. CI chooses whether untrusted source is admitted to any
credential-bearing runner.

An environment with no selected units still gets a valid workflow: its
lifecycle node handles or reports the leaving units. Reports distinguish
failed, blocked, superseded, and skipped dependents; a skipped consumer is never
reported as current merely because it did not run.

## Break-glass and repair

Every CI action has a CLI form, so bootstrapping and recovery never depend on
the pipeline:

| Situation | Path |
| --- | --- |
| First deployment | `nyl state init`, then `nyl ci admit` (or the first dispatcher run); no prior generation is needed |
| Pipeline broken, deploy anyway | `nyl ci admit -e <env>` with dispatcher credentials, then `nyl reconcile -e <env>` locally; in-flight pipelines report superseded |
| Finish a stuck pipeline locally | `nyl reconcile -e <env> [--unit <u>]` converges on the current generation; no admission credentials needed |
| Roll back | `nyl ci admit -e <env> --source <rev> --rollback --reason <text>`, then reconcile in CI or locally |
| Dispatcher unavailable | Set `admission: off` through a reviewed change to the Environment; plain runs then behave as in M3 |
| Lease left by a lost runner | `nyl lease break` and the core [recovery](orchestration-core.md#recovery) rules |
| Leaving units awaiting teardown | `nyl reconcile -e <env> --leaving --allow-teardown`, or `nyl state forget` to release without destroying |
| A unit's native tool needs repair | `nyl hold`, `nyl exec`, a reviewed source change, then `nyl resume` (below) |

`nyl exec -e <env> --unit <u> -- <tool arguments>` runs a unit's native tool
with exactly the resolved configuration, worktree, and admitted credentials a
reconcile would use, under the environment lease, for drivers that offer the
[exec capability](orchestration-core.md#operator-exec). It requires the unit to
be held, so CI cannot race the repair, and records the command, operator, and
reason in a transition commit. Driver-specific options select an alternative
configuration for the repair only, such as OpenTofu's `--backend <file>` for a
separate state location (see the
[OpenTofu repair flow](infrastructure-units.md#repair)). After `nyl resume`, the
next reconcile treats the unit's receipt as stale and re-executes it under its
recovery policy, so the native state and Nyl's receipt converge again.

## Later backend: workers of one run

Parallel effects across CI jobs require an explicit extension of the run model,
not relaxation of the environment lease. A possible interface is `nyl ci
prepare`, `nyl ci execute <unit>`, and `nyl ci finalize`; names and protocol are
unsettled. A coordinator owns the single environment lease and pinned source,
dispatches authorized attempts, accepts checkpoints, and commits final state.
Workers never independently acquire the environment lease or publish final
environment transitions.

Before implementation, specify coordinator liveness across queued/manual jobs,
heartbeat and deadlines, per-attempt claims and fencing, late/duplicate result
admission, approval authority, cancellation and partial failures, recovery after
coordinator loss, and authenticated result transport. A copied manifest or CI
success is not authority to adopt a receipt. Results produced without a valid
attempt fence follow the core's uncertain-execution and recovery rules. Cleanup
must work when an always-run CI finalizer never starts.

The backend must demonstrate the same receipts, freshness decisions, and recovery
semantics as local dependency-wave execution. Until that contract and its failure
scenarios are accepted, only the separate-invocation backend is supported.

## Acceptance scenarios

M3 establishes a graph without effects and proves unit-scoped execution in the
[unit-jobs walkthrough](../nyl/tests/scenarios/walkthroughs/unit-jobs/scenario.yaml).
M8 extends the scenario harness with export/artifact and job-result assertions:

The [admission](../nyl/tests/scenarios/walkthroughs/ci-admission/scenario.yaml)
and [lifecycle](../nyl/tests/scenarios/walkthroughs/ci-lifecycle/scenario.yaml)
walkthroughs use the provider-neutral M8 harness operations defined in the
[scenario format](reference-scenarios.md#m8-ci-conformance-steps).

| Scenario | Required observation |
| --- | --- |
| Two independent producers and one consumer | Stable job IDs and declared edges; one job per unit; lease serializes effects |
| Producer missing at export, then succeeds | Consumer remains in graph and resolves the new receipt when it runs |
| Producer executes but output stays unchanged | Consumer reports current without repeating its effect |
| Producer fails or awaits approval | Independent work can finish; consumer is blocked/skipped and pipeline is non-success |
| Competing invocation changes upstream state | Consumer rechecks freshness, never trusts an old GitLab success |
| Retry after success or uncertain execution | No duplicate successful effect; recovery policy governs uncertainty |
| Checkout does not match the admitted binding | Worker refuses before resolution or driver execution |
| New deployment overtakes queued old work | Every old worker attempt reports superseded without desired/observed writes or driver execution |
| Overtaken pipeline's dispatch is retried | Refused as a rollback; the current generation stays |
| Same dispatch retried | Returns the current generation without minting a new one |
| Explicit rollback to an older source | A new authorized generation, recorded as an override, reconciles the older source |
| Hold, resume, recovery, or a local run intervenes | The generation stays current; a retried job proceeds |
| `state restore` after pushed local transitions | The admission record is carried forward unchanged |
| Unit removed, or selector typo, in reconciliation | Lifecycle job retains or leaves units `pending-teardown` without `--allow-teardown`; nothing is destroyed implicitly |
| MR removes one unit or deselects all units | Planning emits lifecycle consequences, writes no state, and applies only the caller's leaving policy |
| Read-only MR plans with missing outputs | Incomplete downstream plan stays incomplete; no state writes or effects |
| No work, missing report, or canceled worker | Valid summary; no-op is distinct from skipped, superseded, failure, and uncertainty |

Exporter tests parse the generated YAML and cover name collisions, quoting,
provider limits, runner mapping, and absence of secrets. A GitLab integration
check validates dynamic-child configuration, report aggregation, failed/skipped
dependencies, pipeline serialization, and cancellation before declaring support.
The in-memory adapter runs the same graph and admission conformance suite, so
these invariants are tested independently of GitLab scheduling features.
