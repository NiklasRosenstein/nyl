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
Nyl owns identity, resolution, freshness, approvals, and lifecycle. CI owns
event triggers, runner provisioning, credentials, and job presentation.
An adapter exports existing dependencies; it adds no workflow conditionals,
loops, matrices, or alternative readiness rules to unit declarations.

Unit identity remains the literal `metadata.name`, addressed as
`<environment>/<unit>`, independent of its source path. Provider job identifiers
use a deterministic, collision-checked encoding of that address. Reports retain
the original address. Provider-specific images, tags, and job setup belong to a
separate adapter configuration, pinned with the source, not to unit identity or
the execution key. That configuration cannot override dependencies, commands,
approvals, or Nyl's exit categories. Secrets never enter generated YAML.

## Provider-neutral execution contract

The execution graph and manifest are versioned Nyl documents. They carry unit
addresses, dependency edges, operation, immutable source and configuration
bindings, and report locations. They contain no GitLab job IDs, `needs`, stage
names, resource groups, or CI environment-variable names. A provider adapter
maps this model to its scheduler and maps job observations back to the common
report model. Provider pipeline/job identifiers are opaque diagnostic metadata,
not unit identities, deployment generations, or evidence of successful effects.

All adapters use the same core admission, freshness, approval, retry, and
lifecycle rules. A provider advertises capabilities such as generated workflows,
dependency scheduling, always-run aggregation, and pipeline serialization.
Export rejects an unsupported requirement or selects a documented safe fallback;
it must not silently omit jobs or weaken the execution contract. For example,
a provider without dynamic workflow loading can use a checked-in generated
workflow or a dispatcher that submits jobs, while each worker still validates
the same immutable manifest. Provider configuration supplies runner profiles and
event mapping. The first implementation targets GitLab; no GitHub Actions or
other provider support is claimed until its adapter passes conformance tests.

A provider-neutral fixture must export through both the GitLab adapter and an
in-memory test adapter. They must retain identical unit addresses, dependencies,
source bindings, admission requirements, and semantic results. YAML layout and
provider display statuses may differ. Adding a provider must not require new
fields on unit kinds or a provider-specific state/receipt format.

## First backend: separate reconciliation invocations

The first GitLab backend creates one job per selected unit. Each job runs an
ordinary `reconcile --unit` at the same immutable source commit. It is not a
distributed Nyl run: every invocation acquires the environment lease and records
its own transition. Independent jobs may be queued together, but effects within
one environment serialize. No per-unit leases are introduced.

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
  protocol version. Jobs check out that commit and validate the binding before
  invoking Nyl; mutable branch names and ambient checkout state cannot substitute.
- Source is fixed across jobs; observed state is not. Each job rechecks receipts,
  ownership, holds, and approval digests. GitLab `needs` success is scheduling
  evidence only, never authority to execute with stale inputs.
- Every reconciliation attempt, including a retried job, validates its deployment
  admission under the environment lease before desired-state resolution writes,
  result import, or driver execution. A source pin or previous pipeline success
  does not grant admission. See [Deployment admission](#deployment-admission).
- Pipeline serialization is an optional scheduling optimization, not the fence.
  In GitLab a parent trigger can hold a resource group with `strategy: mirror`;
  workers must not acquire that same resource group. Other providers may use
  their own scheduling controls. The core environment lease serializes effects
  regardless of provider, including attempts retried outside the parent job's
  lifetime. This backend promises neither a shared observed snapshot nor one
  atomic transition across jobs.

Lifecycle changes are not assigned implicitly to reconciliation unit jobs. If a
reconciliation export discovers units leaving ownership, pending teardown, or
environment maintenance requiring
mutation outside the named jobs, it refuses with a diagnostic naming the normal
environment-level command. Perform that operation explicitly, with the core's
approvals and teardown flags, then regenerate the graph. The final report job
is read-only and must never reconcile omitted work.

Planning export does not refuse for these conditions. It adds an environment-level
lifecycle plan report alongside unit plan jobs, listing leaving units, reasons,
and consequences even if no units remain selected. It performs no reconciliation
or teardown and writes no desired/observed state. Preserve `plan --fail-on-leaving`
as an explicit caller policy; removal does not fail planning by default. The
aggregate retains this report even when a unit plan is blocked or skipped.

## Deployment admission

A trusted dispatcher admits a reconciliation deployment before workers start.
Under the environment lease it checks deployment policy (including whether the
request is superseded) and records a new `DeploymentAdmission` in protected Nyl
state. The versioned record binds environment/state identity, immutable source,
graph/configuration digest, operation, unique generation ID, monotonic sequence,
and admitted actor/request. The request ID is provider-neutral and opaque; a
GitLab pipeline ID is neither a generation nor its ordering rule. Admission
does not execute units or create success receipts.

Admission uses an expected-current-generation comparison. A dispatcher whose
observation is stale must reload state and repeat policy checks, not blindly
advance it. Repeating the same request returns its generation only while it is
current; a superseded request cannot mint a new generation by retrying. A rollback
is a new explicitly authorized request and generation pointing to older source.
Commit ancestry, branch reachability, and a higher CI job ID do not establish
admission. Provider-specific identity checks run in the dispatcher; the core
accepts normalized authorization evidence through the existing trust model.

Graph extraction/export is read-only. Dispatch computes the semantic graph
digest, admits its immutable binding, then renders/submits provider jobs carrying
that generation. The semantic digest excludes admission IDs, provider job URLs,
and presentation metadata, avoiding a circular digest. Admission cannot be
implicitly renewed by rerunning export. Dispatcher admission authority is kept
separate from worker attempts; running unit-authored commands is not permission
to mint deployment generations. Trusted dispatcher policy must come from
protected configuration, not arbitrary proposed unit source.

Each worker presents the admitted generation and immutable manifest binding.
After acquiring the environment lease, it compares them with the current record
before any desired/observed state writes, checkpoint import, or driver call.
A mismatch fails without those effects, even for a successful job retried after
a newer deployment. Lease acquisition/release may still change coordination refs.
The environment lease prevents a new admission from superseding an active effect;
ordinary lease-loss and uncertain-execution rules still govern interrupted work.
Workers cannot admit themselves or obtain a fresh generation through retry.

Environment-mutating operations outside the admitted worker path invalidate the
current admission under the same lease before changing intent or executing
effects. This includes direct local reconciliation and explicit lifecycle work;
read-only planning/inspection do not invalidate it. State moves preserve the
admission fence, and fresh state identities/generation IDs never reuse old ones.
An admitted worker rechecks normal approvals independently: admission neither
grants plan approval nor permits teardown.

Implement the admission record and gate in the orchestration/state layers as an
M8 extension. Read-only planning needs no deployment admission or state writes.
Providers can implement admission with a bootstrap dispatcher and pass the public
binding to workers, but possession of that binding grants no additional state
permissions. Worker execution must use the core gate, rather than a shell check
before a separate `reconcile` that could race a newer admission.

### Proposed GitLab flow

```sh
# Read-only graph export at the pinned source checkout:
nyl ci export gitlab -e config --source <full-commit> \
  --operation reconcile --output pipeline.yml

# Trusted dispatch admits the graph, then binds the resulting generation into
# the rendered provider jobs before submission (admission CLI spelling is TBD).

# A generated unit job; invocation includes the graph-binding validation:
nyl reconcile -e config --source <full-commit> --unit kalix-talos \
  --admission <generation-id> --wait-lease 30m --output json
```

The parent uploads `pipeline.yml` and the public graph manifest, then starts a
dynamic child pipeline using the artifact and `strategy: mirror`. Generated
includes use literal paths. The adapter renders validated argument vectors and
YAML rather than interpolating names or authored text into shell fragments.
Runner setup admits credentials through the normal driver/secret contracts;
GitLab artifacts and dotenv files do not transport authoritative unit outputs.

Each worker stores the typed operation report as an artifact even on failure;
diagnostics and driver transcripts remain in that unit's job log. Reports carry
unit address, source commit, Nyl run ID, and the CI pipeline/job URL. A summary
job runs even after failures, includes skipped or missing jobs, and links to
their reports. GitLab artifact retention is for observability; Nyl state and
receipts remain authoritative and survive artifact expiry.

| Nyl result | CI behavior |
| --- | --- |
| Current or executed successfully (0) | Success, identifying no-op versus execution |
| Configuration/operational error (1) | Fail with the core diagnostic |
| Waiting, blocked, or lease wait exhausted (2) | Non-success with blocker and next action; never silently allowed to fail |
| Driver failure (3) | Fail, retaining the unit report |
| Uncertain or lease lost (4) | Fail with recovery guidance; no unconditional automatic retry |

GitLab's generic retry policy must not blindly repeat effects. A manual job retry
starts a fresh ordinary invocation at the same source. The core checks current
admission before receipts and its recovery policy. A superseded job fails before
effects; within a current generation successful work becomes a no-op and an
uncertain non-idempotent command still requires recovery. CI manual buttons do
not grant approval: the adapter supplies evidence through the normal approval
contract and Nyl revalidates its binding at execution time.

For merge requests, export `--operation plan`. Jobs invoke only read-only
`plan --unit`; the adapter never reconciles a producer to complete a plan.
First-time downstream plans can be incomplete because no upstream output exists.
Report exit 2 and its blockers honestly; a successful upstream plan does not
create a receipt or provide executable preview values. Plan jobs use read-only
state access and the driver's required planning credentials, not reconciliation
credentials or production state writes. CI chooses whether untrusted source is
admitted to any credential-bearing runner.

An environment with no selected units still gets a valid report-only workflow.
For reconciliation it succeeds only if the ownership/lifecycle check finds no
work left behind; for planning it reports removals and follows the caller's
`--fail-on-leaving` policy. Reports distinguish failed, blocked, and skipped
dependents; a skipped consumer is never reported as current merely because it
did not run.

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

The [admission retry](../nyl/tests/scenarios/walkthroughs/ci-admission/scenario.yaml)
and [lifecycle planning](../nyl/tests/scenarios/walkthroughs/ci-lifecycle/scenario.yaml)
walkthroughs use the provider-neutral M8 harness operations defined in the
[scenario format](reference-scenarios.md#m8-ci-conformance-steps). They are
acceptance fixtures, not executable tests before that harness extension lands.

| Scenario | Required observation |
| --- | --- |
| Two independent producers and one consumer | Stable job IDs and declared edges; one job per unit; lease serializes effects |
| Producer missing at export, then succeeds | Consumer remains in graph and resolves the new receipt when it runs |
| Producer executes but output stays unchanged | Consumer reports current without repeating its effect |
| Producer fails or awaits approval | Independent work can finish; consumer is blocked/skipped and pipeline is non-success |
| Competing invocation changes upstream state | Consumer rechecks freshness, never trusts an old GitLab success |
| Retry after success or uncertain execution | No duplicate successful effect; recovery policy governs uncertainty |
| Source or graph binding mismatch | Worker refuses before driver execution |
| New deployment overtakes queued old work | Every old worker attempt is rejected by the core admission gate |
| A finishes, B deploys, then an A job is retried | A's old generation is rejected without desired/observed writes or driver execution |
| Explicit rollback to A's source after B | A new authorized request/generation permits the rollback; retrying A's old request does not |
| Admission request races or is retried | Expected-generation check and policy revalidation prevent stale admission; retry cannot renew superseded authority |
| Direct local reconciliation or lifecycle work intervenes | Current admission is invalidated under the lease; old workers must be redispatched |
| Selector typo, removal, pending teardown in reconciliation | Export refuses; no implicit teardown or false empty-workflow success |
| MR removes one unit or deselects all units | Planning emits lifecycle consequences, writes no state, and applies only the caller's leaving policy |
| Read-only MR plans with missing outputs | Incomplete downstream plan stays incomplete; no state writes or effects |
| No work, missing report, or canceled worker | Valid summary; no-op is distinct from skipped, failure, and uncertainty |

Exporter tests parse the generated YAML and cover name collisions, quoting,
provider limits, runner mapping, and absence of secrets. A GitLab integration
check validates dynamic-child configuration, report aggregation, failed/skipped
dependencies, pipeline serialization, and cancellation before declaring support.
The in-memory adapter runs the same graph and admission conformance suite, so
these invariants are tested independently of GitLab scheduling features.
