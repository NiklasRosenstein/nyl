# CI execution adapters

**Status:** planned direction. M3 provides a backend-neutral graph and reports;
M8 adds an opt-in GitLab adapter. Commands below are proposed interfaces, not
current CLI capabilities. See [ROADMAP.md](../ROADMAP.md), the
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
- The deployment integration serializes whole child pipelines for an environment,
  rather than only individual jobs, and refuses a superseded source before
  admitting its pipeline. Cancellation alone cannot fence effects already running.
  GitLab can hold a resource group on the parent trigger with `strategy: mirror`;
  workers must not acquire that same resource group. Nyl's environment lease
  still protects against local and other runners. This backend promises neither
  a shared observed snapshot nor one atomic transition across its jobs.

Lifecycle changes are not assigned implicitly to unit jobs. If export discovers
units leaving ownership, pending teardown, or environment maintenance requiring
mutation outside the named jobs, it refuses with a diagnostic naming the normal
environment-level command. Perform that operation explicitly, with the core's
approvals and teardown flags, then regenerate the graph. The final report job
is read-only and must never reconcile omitted work.

### Proposed GitLab flow

```sh
# Bootstrap job, at the pinned source checkout:
nyl ci export gitlab -e config --source <full-commit> \
  --operation reconcile --output pipeline.yml

# A generated unit job; invocation includes the graph-binding validation:
nyl reconcile -e config --source <full-commit> --unit kalix-talos \
  --wait-lease 30m --output json
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
receipts and its recovery policy, so successful work becomes a no-op and an
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

An environment with no selected units still gets a valid report-only child
pipeline. It succeeds only if the full ownership/lifecycle check finds no work
left behind. Reports distinguish failed, blocked, and skipped dependents; a
skipped consumer is never reported as current merely because it did not run.

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

| Scenario | Required observation |
| --- | --- |
| Two independent producers and one consumer | Stable job IDs and declared edges; one job per unit; lease serializes effects |
| Producer missing at export, then succeeds | Consumer remains in graph and resolves the new receipt when it runs |
| Producer executes but output stays unchanged | Consumer reports current without repeating its effect |
| Producer fails or awaits approval | Independent work can finish; consumer is blocked/skipped and pipeline is non-success |
| Competing invocation changes upstream state | Consumer rechecks freshness, never trusts an old GitLab success |
| Retry after success or uncertain execution | No duplicate successful effect; recovery policy governs uncertainty |
| Source or graph binding mismatch | Worker refuses before driver execution |
| New pipeline overtakes queued old work | Deployment admission rejects superseded source; old pipeline cannot resume deployment |
| Selector typo, removal, pending teardown | Export refuses; no implicit teardown or false empty-pipeline success |
| Read-only MR plans with missing outputs | Incomplete downstream plan stays incomplete; no state writes or effects |
| No work, missing report, or canceled worker | Valid summary; no-op is distinct from skipped, failure, and uncertainty |

Exporter tests parse the generated YAML and cover name collisions, quoting,
provider limits, runner mapping, and absence of secrets. A GitLab integration
check validates dynamic-child configuration, report aggregation, failed/skipped
dependencies, pipeline serialization, and cancellation before declaring support.
