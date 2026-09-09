# Nyl orchestration roadmap

## Purpose and use

This roadmap frames Nyl's expansion from Kubernetes manifest generation and
rendered GitOps into general infrastructure rendering, state management,
reconciliation, and execution through drivers. It guides work across milestones;
it is not a complete specification, a release schedule, or a commitment to every
proposed interface.

The working direction is to expose orchestration through Nyl, reuse
[gitopsctr](https://github.com/NiklasRosenstein/gitopsctr)'s lifecycle semantics and
implementation initially, and decide implementation consolidation after a useful
workflow demonstrates the contracts and recovery behavior.

Key architectural departures require a focused second check against evidence and
user goals, followed by an update to this document. Record the selected direction
and its rationale here; keep superseded plans and decision history in commits,
pull requests, or dedicated decision notes. [AGENTS.md](AGENTS.md) defines the
working instructions.

## Progress and next step

The feasibility assessment is complete. Orchestration integration and the
interfaces below are planned capabilities, not claims about Nyl's current CLI.

| ID | Milestone | Status | Depends on |
| --- | --- | --- | --- |
| M1 | Shared resource, lifecycle, and interface contract | Planned | — |
| M2 | Nyl orchestration interface over one backend | Planned | M1 |
| M3 | Host identity to Terraform trust registration | Planned | M2 |
| M4 | NixOS deployment and lifecycle recovery | Planned | M3 |
| M5 | Distribution and implementation consolidation decision | Planned | M4 |

**Next step:** inventory the relevant Nyl and gitopsctr contracts and produce the
M1 contract, resolving the first-workflow questions under Open decisions. Begin
with the minimum coherent model needed for M3, including its recovery semantics.

## Product direction

> Authoring produces desired resources and artifacts. Reconciliation coordinates
> managed units. Drivers implement each unit's lifecycle.

Nyl provides one authoring and operational experience across native infrastructure
tools. Kubernetes remains a straightforward use case with an independently usable
rendering path. Cross-system orchestration explicitly introduces units,
dependencies, references, and recorded evidence.

An orchestration unit is a meaningful lifecycle boundary: a Terraform
configuration, Kubernetes release, host identity, or NixOS deployment. Native
tools retain responsibility for the resources inside that boundary. The core
does not reproduce Terraform's individual resource graph or Kubernetes's
controllers.

A reusable component may compose host identity, trust registration, and NixOS
deployment units without requiring a domain-specific controller such as
`BaoBackup`. If the composition needs behavior that its primitives cannot
express, that lifecycle behavior must be modeled explicitly; templating alone
cannot provide it.

### Initial scope

- Reusable authoring, explicit inputs, immutable desired units, and artifacts.
- Dependency-aware execution and typed references to recorded public outputs.
- Unit identity, ownership, execution coordination, evidence, and recovery.
- Consistent local and CI commands for planning, publication, reconciliation,
  inspection, verification, and deletion intent.
- Host enrollment, Terraform trust registration, and NixOS deployment as the
  first cross-system proof, with ordinary Kubernetes workflows remaining usable.

### Outside the initial scope

- A general CI pipeline or arbitrary workflow language.
- A universal resource database replacing native state backends.
- Cross-system transactions or a guarantee of reversible external effects.
- A web dashboard, hosted service, or highly available controller fleet.
- An immediate rewrite of gitopsctr or generalization of every Nyl renderer API.
- Broad driver coverage, promotion policy redesign, or a plugin marketplace.

A command invoked locally or in CI is the initial execution model. Continuous
GitOps additionally needs automatic retrieval and repeated observation and
reconciliation; a scheduled runner or service can provide that operating model.

## Architectural frame

### Responsibilities and integration

| Layer | Responsibility |
| --- | --- |
| Authoring | Components, templates, composition parameters, native source files |
| Compilation | Resolve explicit inputs into units and immutable artifacts, retaining unresolved composition intent |
| Orchestration core | Identity, membership, dependencies, references, scheduling, lifecycle fences, and evidence relationships |
| Drivers | Native planning, execution, readiness, verification, inventory, and supported teardown |
| Storage | Desired snapshots, receipts, artifact descriptors, and execution coordination records |
| Interfaces | One consistent CLI and machine interface over core operations and observations |

The first integration uses a narrow, versioned process contract around complete
gitopsctr controller operations. One backend owns desired/observed state,
projection, leases, receipt freshness, and lifecycle decisions. Nyl supplies the
user interface and authoring integration; it does not independently reconstruct
those decisions or call drivers under a second scheduler.

gitopsctr's Python driver classes are not a language-neutral protocol. Protocol
design, compatible version selection, packaging, and recovery testing are real
integration work. A dual-runtime distribution is acceptable for the initial
proof, subject to the M5 decision.

### Resource model and scope

| Concept | Meaning |
| --- | --- |
| Project | Authoring and configuration boundary |
| Environment | Orchestration namespace and policy boundary |
| Composition | Reusable declaration and ownership of related units |
| Unit | Independently identified lifecycle boundary |
| DeploymentTarget | Kubernetes rendering/publication configuration referenced by relevant units |
| Execution destination | Driver-specific cluster, backend/workspace, or host |

An environment may contain several Kubernetes targets, Terraform configurations,
and hosts. It does not require a Kubernetes destination. Keep DeploymentTarget's
Kubernetes meaning rather than making non-Kubernetes units supply cluster or
Argo CD configuration.

Kubernetes compiler resources use `k8s.gitops.nyl/v1`; HelmChart and
RemoteManifest use `k8s.nyl/v1`, and chart-backed component invocations use
`components.k8s.nyl/v1`. Shared GitRepository resources use `gitops.nyl/v1`.
Resource reference pages and catalog summaries derive from the Rust-generated
JSON Schemas. M1 defines additional orchestration APIs independently of these
Kubernetes contracts.

Clusters can explicitly borrow CRD schemas or the complete Kubernetes API
contract from another declared Cluster. Effective capabilities drive both
rendering and validation; destinations, values, and live connection settings
remain local. `nyl capture cluster` refreshes committed capabilities and optional
CRD schema snapshots. Project-configured validators check final artifacts before
render output, diff calculation, application, or publication. These operations
remain independently usable without orchestration state.

Separate authoring membership, unit identity, and native resource ownership.
Stable identities and incarnation fences protect against stale operations after
deletion and recreation. Different unit names do not imply disjoint native
ownership: validate backend/workspace, cluster/resource, and host scopes where
the driver can establish them.

### Rendering, references, and artifacts

The execution flow is:

```text
source and components -> composition -> desired units and artifacts
                                            |
                                            v
                                   execute ready units
                                            |
                                            v
                                 receipts and public outputs
                                            |
                                            v
                           resolve dependent inputs and repeat
```

Ordinary `nyl render` remains a rendering operation and requires no orchestration
store. It must not perform enrollment or deployment, or implicitly read the
latest receipt. Orchestration-dependent rendering receives an explicit, pinned
input snapshot.

Typed references expose dependency edges and identify exact producer evidence.
Missing or stale evidence blocks consumers. Invalid evidence fails validation.
Do not hide runtime dependencies inside opaque template lookups.

Retain durable composition intent when upstream outputs are unavailable. Resolve
and materialize dependent units as evidence becomes available, without rereading
unrelated mutable source. Bound each run's source intent and define how generated
desired revisions advance within that run.

Artifacts can be typed documents or file trees. Kubernetes normalization,
namespace handling, deduplication, and policy processing belong to the Kubernetes
path. Terraform and Nix files may be referenced and packaged in their native
formats. Every input affecting rendered bytes participates in dependency
recording; cache entries are disposable and never establish execution success.

### State, evidence, and recovery

| State | Authority |
| --- | --- |
| Authored intent | Source Git |
| Resolved units and immutable payload descriptors | Desired-state Git |
| Receipts and deliberately public outputs | Observed-state Git |
| Terraform resource state | Terraform/OpenTofu backend |
| Private host keys and credentials | Host or secret store |
| Render cache | Disposable local storage |
| Execution coordination | Explicit claims/leases and recovery records |

Preserve the distinction between rendered-file ownership, published desired
state, successful execution evidence, and current external observations. A
matching receipt proves success for particular inputs; it does not prove the
system remains healthy or free of drift.

Keep large artifacts outside Git when appropriate, with immutable descriptors
and integrity checks. Persist only outputs explicitly admitted as public. Reject
sensitive native-tool outputs from public receipts, and keep credentials and
private keys out of desired state, public artifacts, and diagnostic transcripts.

The lifecycle contract must address:

- Effects that succeed before receipt publication fails: inspect or resume
  safely; missing evidence does not imply that nothing happened.
- Competing or stale runners: Git compare-and-swap protects publication, while
  execution leases and native locks protect effects. Lease loss must have an
  explicit cancellation and recovery policy.
- Timeouts and disconnected processes: represent uncertain completion and
  recover before blindly repeating an operation.
- Partial input: omission requests deletion only within a declared authoritative
  ownership set; selecting a subset is not an instruction to destroy the rest.
- Deletion: retain sufficient desired inputs and evidence for retry-safe teardown
  and fence it against a new incarnation of the same name.
- Ordering: creation dependencies do not automatically define safe rotation or
  decommissioning order. Unsupported teardown remains visibly blocked.
- Rollback: publish forward desired intent and reconcile it; do not promise
  universal reversal of effects or rewind observed history.

### Execution ownership

Kubernetes delivery distinguishes publication for an external reconciler,
publication plus observation, and direct execution through Nyl. A unit's mode
makes the executor explicit. Observation of Argo CD does not authorize Nyl to
apply or prune its workloads.

Dependencies distinguish artifacts being published, API acceptance, and observed
readiness. A publication-only unit cannot satisfy a dependency that requires a
healthy deployment. Ownership changes require a deliberate handoff.

Validate conflicts within the inventory Nyl and its drivers can establish; do not
claim automatic discovery of every independent external manager. Reusing Nyl's
direct Kubernetes lifecycle requires explicit compatibility decisions about
release history, pruning, and readiness.

## User and machine interfaces

Preserve `nyl render`, `diff`, `apply`, `render-tree`, and `publish-tree` with their
Kubernetes meanings. Ordinary usage must not require understanding receipts,
execution leases, or orchestration storage.

The working CLI design uses an explicit `nyl orchestrate` group. Names and flags
are proposals to validate in M1/M2:

```bash
nyl orchestrate plan --environment production -f platform.yaml
nyl orchestrate publish --environment production -f platform.yaml
nyl orchestrate reconcile --environment production
nyl orchestrate status --environment production
nyl orchestrate verify --environment production
```

| Operation | Contract |
| --- | --- |
| plan | Preview effects and unresolved inputs without deploying |
| publish | Record desired state; an authorized runner may subsequently execute it |
| reconcile | Drive eligible work from published intent through bounded dependency waves |
| status / get | Inspect intent, evidence, ownership, progress, and blockers |
| verify | Observe external state and report drift |
| delete | Record explicit deletion intent for reconciliation |

Avoid a second meaning for `apply`. Expose both `reconcile` and `converge` only if
their distinction provides a demonstrated operator benefit. Specify command
effects, selection defaults, non-interactive behavior, deadlines, and exit
categories before stabilizing the CLI.

A plan with unavailable upstream outputs is incomplete and must say so. Preview
values cannot become deployment inputs. Approval policy must define whether
newly resolved downstream plans execute automatically or require further review.
Do not promise execution of an approved native plan unless the driver preserves
and validates that exact plan and its input/state preconditions.

Inspection explains desired revision, matching or stale evidence, the precise
dependency blocking progress, execution owner, and when external state was last
verified. Do not collapse recorded success and current health into one ambiguous
status. Human tables and machine output derive from the same backend snapshot.

The machine contract includes versioned requests/results, capability negotiation,
structured diagnostics and progress, operation identity, cancellation behavior,
and explicit source/desired/observed revision selection. Keep machine output on
stdout and human diagnostics on stderr. A future UI consumes the same contract;
it must not implement another interpretation of lifecycle state.

Drivers advertise materialization, planning, reconciliation, verification, and
teardown capabilities. Inputs include resolved values, artifact/source digests,
execution context, previous evidence, and lifecycle fences. Results distinguish
success, pending, unsupported behavior, failure, and uncertain completion. Keep
resource schema, process protocol, and driver implementation versions separate.

## Milestones and acceptance criteria

### M1 — Shared resource, lifecycle, and interface contract

- [ ] Inventory the relevant Nyl and gitopsctr APIs and persistence contracts;
  distinguish reusable behavior from compatibility gaps.
- [ ] Define identity, ownership scopes, typed references, evidence freshness,
  artifact contracts, lifecycle states, and deletion/recovery invariants.
- [ ] Specify the minimum process protocol and CLI effects, including revision
  selection, partial plans, cancellation, and execution authorization.
- [ ] Resolve the bootstrap identity, trust system, private-key custody, and
  Terraform/OpenTofu choice for the first workflow.

**Exit criterion:** the contract can explain a successful run, an unavailable
upstream output, effects without a receipt, competing runners, and deletion
without relying on hidden state or ambiguous command semantics.

### M2 — Nyl orchestration interface over one backend

- [ ] Implement the versioned backend adapter and explicit orchestration group.
- [ ] Connect Nyl authoring to canonical composition inputs without duplicating
  lifecycle decisions in Rust.
- [ ] Provide structured inspection, actionable blockers, and local/CI parity.
- [ ] Keep backend installation optional for ordinary Kubernetes rendering.
- [ ] Establish compatibility fixtures and meaningful protocol-boundary tests.

**Exit criterion:** Nyl can publish, plan, reconcile, inspect, and verify a small
composition through one backend, and ordinary Kubernetes workflows retain their
interfaces and independent rendering path.

### M3 — Host identity to Terraform trust registration

- [ ] Implement authenticated enrollment with stable identity and private-key
  custody that survives interrupted execution.
- [ ] Export a typed public identity and resolve it into a trust-registration
  unit using exact producer evidence.
- [ ] Preserve native Terraform/OpenTofu state and locking; enforce public-output
  admission and explain incomplete plans.
- [ ] Prove safe retries, stale-evidence blocking, competing-runner behavior, and
  recovery after effects succeed but publication fails.

**Exit criterion:** a reusable composition enrolls a host and registers its trust
through Nyl; repeat execution converges without accidental identity replacement
or duplicate ownership, and interruption has a demonstrated recovery path.

### M4 — NixOS deployment and lifecycle recovery

- [ ] Add a NixOS unit consuming the required identity and trust evidence, with
  an explicit deployment mechanism and readiness checks.
- [ ] Exercise key rotation, partial failure, disconnected execution, failed
  evidence publication, and recovery across the full composition.
- [ ] Define and verify decommissioning and rollback limits, retaining blocked
  intent when a driver cannot safely complete teardown.
- [ ] Verify direct versus external Kubernetes execution boundaries alongside
  the cross-system workflow.
- [ ] Document an end-to-end local/CI example and operational recovery actions.

**Exit criterion:** host enrollment, trust registration, and NixOS deployment
work as one observable composition whose update, interruption, rotation, and
deletion behavior is supported by tests or reproducible acceptance exercises.

### M5 — Distribution and implementation consolidation decision

- [ ] Evaluate installation, dual-runtime support, performance, protocol
  stability, maintenance cost, and driver extensibility using M2–M4 evidence.
- [ ] Select the implementation and distribution direction and record its
  rationale and constraints in this roadmap.
- [ ] If a port is selected, define behavioral parity and persisted-state
  compatibility criteria before scheduling it.
- [ ] Set the next scope based on demonstrated needs, including whether a
  continuous runner or additional drivers warrant investment.

**Exit criterion:** the selected architecture has an explicit support and
distribution model, and any consolidation has measurable compatibility gates.
Completing this milestone does not require a Rust port.

## Open decisions

Resolve these at the milestone where they affect implementation; they are not
reasons to delay independent work.

| Decision | Needed by |
| --- | --- |
| Composition schema and mapping to gitopsctr Stack/Unit contracts | M1 |
| Environment configuration and relationship to existing project/target configuration | M1 |
| Source, desired, observed, and coordination ref layout and authorization | M1 |
| Enrollment trust root, key purpose/custody, and trust-registration destination | M1 |
| Terraform versus OpenTofu executable support and native plan approval semantics | M1/M3 |
| Dependency readiness, evidence freshness, and automatic downstream execution policy | M1/M3 |
| Process protocol transport, cancellation guarantees, and supported backend versions | M2 |
| NixOS deployment mechanism, rotation, and decommissioning behavior | M4 |
| Single-binary requirement versus an optional orchestration runtime | M5 |
| Continuous runner ownership, observation cadence, and drift-repair policy | After M4 |

## Implementation reference points

- [Nyl rendering session and bundle](nyl/src/render/session.rs)
- [Nyl components](nyl/src/components/mod.rs)
- [Kubernetes GitOps resource model](nyl/src/resources/gitops.rs)
- [Rendered-file ownership reconciliation](nyl/src/gitops/reconcile.rs)
- [Rendered-tree publication](nyl/src/cli/commands/publish_tree.rs)
- [Direct Kubernetes application](nyl/src/cli/commands/apply.rs)
- [Kubernetes release state](nyl/src/kubernetes/state.rs)
- [gitopsctr concepts](https://github.com/NiklasRosenstein/gitopsctr/blob/main/docs/concepts.md)
- [gitopsctr driver contracts](https://github.com/NiklasRosenstein/gitopsctr/blob/main/src/gitopsctr/driver.py)
- [gitopsctr Kubernetes delivery](https://github.com/NiklasRosenstein/gitopsctr/blob/main/docs/drivers/kubernetes-manifests.md)

These are navigation aids, not frozen API guarantees. Verify the implementation
revision used by an integration before depending on its behavior.
