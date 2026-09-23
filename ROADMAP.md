# Nyl orchestration roadmap

## Purpose and use

This roadmap frames Nyl's expansion from Kubernetes manifest generation and
rendered GitOps into GitOps orchestration across infrastructure tools: container
image builds, Terraform/OpenTofu configurations, and Kubernetes manifests
rendered from their outputs. It guides work across milestones; it is not a
complete specification, a release schedule, or a commitment to every proposed
interface.

Nyl implements the orchestration semantics natively, in Rust, as one tool with
one state model. Every orchestration capability is additive: existing commands,
resources, and rendered GitOps behavior keep their meaning, and a project that
never declares an orchestration resource never needs to understand one.

Key architectural departures require a focused second check against evidence and
user goals, followed by an update to this document. Record the selected direction
and its rationale here; keep superseded plans and decision history in commits,
pull requests, or dedicated decision notes. [AGENTS.md](AGENTS.md) defines the
working instructions.

## Progress and next step

Orchestration and the interfaces below are planned capabilities, not claims
about Nyl's current CLI.

| ID | Milestone | Status | Depends on |
| --- | --- | --- | --- |
| M1 | Unit, input, state, and promotion contract | Planned | — |
| M2 | Release inputs without orchestration | Planned | M1 |
| M3 | Orchestration core with a constrained command unit | Planned | M1 |
| M4 | Container image and Terraform units | Planned | M3 |
| M5 | Images and Terraform outputs into Kubernetes releases | Planned | M2, M4 |
| M6 | Promotion paths | Planned | M5 |
| M7 | Continuous operation and scope decision | Planned | M6 |

**Next step:** the [Release inputs contract](design/release-inputs.md) settles
M2's design, so M2 implementation can start. In parallel, continue M1 with the
unit, state, and environment contract.

## Product direction

> Authoring declares units and how their inputs are bound. Reconciliation runs
> ready units and records their evidence. Promotion copies selected, proven
> values from one environment to another through an explicit path.

An orchestration unit is a meaningful lifecycle boundary: an image build, a
Terraform configuration, a command, or one Kubernetes publication. A unit takes
declared inputs, performs its effect through a driver, and records declared
public outputs and artifact descriptors. Native tools retain responsibility for
the resources inside that boundary; Nyl does not reproduce Terraform's resource
graph or Kubernetes controllers.

The target workflow is: build an image, apply Terraform, and render Kubernetes
manifests that consume the image digest and Terraform outputs, then promote the
exact values proven in one environment to the next.

### Principles

- **Additive.** `render`, `diff`, `apply`, `render-tree`, `publish-tree`, and
  their resources keep their current behavior. New fields are optional and have
  no effect when omitted.
- **Gradual.** Release inputs work with static values or pinned external state
  and no orchestration. Orchestration is one more way to bind the same inputs.
- **Explicit links.** Dependencies and promotions are declared edges between
  named selectors. Nothing is inferred from matching names or shapes.
- **Pinned evidence.** Every value that reaches an effect is traceable to an
  exact source commit, state revision, or recorded unit result.

### Initial scope

- Declared, typed Release inputs bound to static values, project files, pinned
  external Git state, or state in the target's own publication branch.
- Units with declared inputs, public outputs, and artifact descriptors; a
  constrained command unit; container image and Terraform units; a Kubernetes
  publication unit over the existing rendered GitOps path.
- Dependency-aware execution with typed references to recorded outputs.
- Unit identity, execution coordination, evidence freshness, and recovery.
- Explicit promotion paths between environments with recorded lineage.
- Consistent local and CI commands for planning, reconciliation, inspection,
  verification, promotion, and deletion intent.

### Outside the initial scope

- A general CI pipeline or workflow language: no conditionals, loops, matrix
  expansion, or ordering beyond the dependency graph.
- A universal resource database replacing native state backends.
- Cross-system transactions or a guarantee of reversible external effects.
- Promotion of native tool state; each environment keeps its own Terraform
  backend and lock.
- A web dashboard, hosted service, or highly available controller fleet.
- Broad driver coverage or a plugin marketplace.

A command invoked locally or in CI is the initial execution model. Events such
as "a new dev receipt was published" trigger CI jobs that call Nyl; the core
stores no event triggers. Continuous observation is an M7 decision.

## Architectural frame

### Layers

| Layer | Responsibility |
| --- | --- |
| Authoring | Components, templates, Release inputs, units, promotion paths, native source files |
| Resolution | Bind inputs to pinned values and produce immutable desired units |
| Orchestration core | Identity, dependencies, references, scheduling, leases, evidence, promotion |
| Drivers | Native planning, execution, output capture, verification, and supported teardown |
| Storage | Desired snapshots, receipts, artifact descriptors, promotion records, coordination |
| Interfaces | One CLI and machine interface over the same operations and observations |

Drivers are Rust implementations behind one internal trait with advertised
capabilities (plan, reconcile, verify, teardown). The command unit is the
extension point for tools without a dedicated driver. An out-of-process driver
protocol is not part of the initial scope.

### Resource model and scope

| Concept | Meaning |
| --- | --- |
| Project | Authoring and configuration boundary |
| Environment | Orchestration namespace: its units, state refs, and promotion policy |
| Unit | Independently identified lifecycle boundary within one environment |
| DeploymentTarget | Kubernetes rendering and publication configuration, unchanged |
| PromotionPath | Declared mapping of selected values from a source environment to a target environment |
| Execution destination | Driver-specific registry, backend/workspace, or cluster |

An environment may contain several image builds, Terraform configurations, and
Kubernetes publication units. It does not require a Kubernetes destination.
DeploymentTarget keeps its Kubernetes meaning; non-Kubernetes units never
supply cluster or Argo CD configuration.

Orchestration resources use a new API group, proposed as `orchestration.nyl/v1`,
defined independently of the Kubernetes contracts. Kubernetes compiler resources
keep `k8s.gitops.nyl/v1`; HelmChart and RemoteManifest keep `k8s.nyl/v1`;
chart-backed component invocations keep `components.k8s.nyl/v1`; shared
GitRepository resources keep `gitops.nyl/v1`. Resource reference pages derive
from the Rust-generated JSON Schemas.

Separate authoring membership, unit identity, and native resource ownership.
Stable identities and incarnation fences protect against stale operations after
deletion and recreation. Different unit names do not imply disjoint native
ownership: validate backend/workspace, registry repository, and cluster/resource
scopes where the driver can establish them.

### Release inputs (M2)

A Release may declare named, typed inputs. Templates read them as
`inputs.<name>`, alongside the existing `values`. A Release without declared
inputs renders exactly as today. The draft contract is
[design/release-inputs.md](design/release-inputs.md).

Inputs use a small closed type set (`string`, `integer`, `number`, `boolean`,
`object`, `array`, with optional `default` and scalar `enum`). It needs no new
validator, keeps source/target compatibility checks for promotion decidable,
and is a JSON Schema subset, so a richer schema can be added later. Deeper
structure is left to downstream validators such as chart value schemas.

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: Release
metadata: {name: web, namespace: web}
spec:
  inputs:
    image: {type: string, description: Immutable image reference}
    databaseHost: {type: string}
    replicas: {type: integer, default: 2}
```

A Release that declares inputs must have a literal `metadata.name` and a literal
`spec.inputs` block, like the existing static Release envelope. Declarations are
then known at discovery time, before the file is rendered, so Nyl can build the
`inputs` context from bindings and check required inputs and types without a
second rendering pass.

Bindings live on the DeploymentTarget, which is static and already carries
per-target values; Release defaults cover environment-independent values.
Group-level defaults can be added later if repetition across targets proves to
be a problem. Bindings are keyed by the Release's rendered
identity, `<applicationGroup>/<release>`, because one Release name can appear in
several groups on one target. Unknown keys, unknown input names, and duplicate
bindings are errors. Each binding selects exactly one source:

| Binding | Resolves from | Needs orchestration |
| --- | --- | --- |
| `value` | Inline static value | No |
| `fromFile` | Project file plus JSON Pointer | No |
| `fromGit` | GitRepository, human `revision`, locked `commit`, path, JSON Pointer | No |
| `fromPublication` | State file in the target's own publication branch at the publication base commit | No |
| `fromUnit` | Recorded public output of a unit in the same environment | Yes (M5) |
| `fromPromotion` | A value recorded by a PromotionPath into this environment | Yes (M6) |

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {name: dev}
spec:
  # existing fields unchanged
  releaseInputs:
    platform/web:
      image:
        value: registry.example.com/web@sha256:…
      databaseHost:
        fromGit:
          repositoryRef: {name: platform-state}
          revision: main
          commit: 3f1c…            # refreshed by a lock-update command
          path: database/outputs.json
          pointer: /host
```

- `fromGit` follows the ApplicationGroup source-lock pattern: rendering reads
  only the locked commit, and `nyl update source-locks` refreshes these locks
  together with ApplicationGroup locks, grouped by repository and revision.
- `fromPublication` supports write-back workflows where an external tool commits
  state to the deploy branch. `publish-tree` reads the file at the branch head it
  builds on and pushes with a compare-and-swap, so the published commit holds
  the state and the manifests rendered from it, and the index records the state
  digest. The file must stay outside Argo CD-synced directories. With `carry`,
  the state file is instead produced uncommitted in the working tree and
  written by `publish-tree` itself, alongside the manifests derived from it.
- Rendering validates bound values against declared types and fails on an
  unbound input without a default. Inputs are opt-in by the Release author, so
  this never affects an existing Release.
- Direct `nyl render`, `diff`, and `apply` apply the selected target's bindings,
  so they match `render-tree`, and accept `--input`/`--inputs` overrides that
  tree commands never accept.
- Remote ApplicationGroups receive inputs exactly like local groups, as they
  already receive target `values`; the platform's binding key is the admission.
- Resolved inputs participate in the dependency recorder and render-cache key.
  The rendered ownership index records a digest and provenance per input in its
  existing `inputs` map, not raw values, and without a format version change;
  published trees remain reconcilable across the upgrade.
- `fromUnit` and `fromPromotion` bindings are rejected by ordinary
  `render-tree`. Only orchestrated execution may resolve them, and it passes
  them to rendering as an explicit, pinned input snapshot. Ordinary rendering
  never reads receipts implicitly, and runtime dependencies never hide inside
  opaque template lookups.

### Units, references, and artifacts (M3–M5)

Units are declared per environment:

```yaml
apiVersion: orchestration.nyl/v1
kind: Unit
metadata: {name: web-image}
spec:
  environment: dev
  driver: OciImage
  inputs:
    context: {path: services/web}
  outputs: [digestRef]
---
apiVersion: orchestration.nyl/v1
kind: Unit
metadata: {name: database}
spec:
  environment: dev
  driver: Terraform
  inputs:
    source: {path: infra/database}
    variables:
      vpcId: {fromUnit: {unit: network, output: /vpcId}}
  outputs: [host, port]
---
apiVersion: orchestration.nyl/v1
kind: Unit
metadata: {name: kubernetes}
spec:
  environment: dev
  driver: KubernetesPublication
  inputs:
    target: dev          # DeploymentTarget whose Release inputs use fromUnit
```

A Kubernetes publication unit places its target into the unit's environment,
and that target's `fromUnit` and `fromPromotion` bindings resolve there.
DeploymentTarget itself stays unchanged. A target that no publication unit
references rejects those bindings; a target referenced from two environments is
an error. Several targets, and therefore several environments, may share one
Cluster.

- A `fromUnit` reference is a dependency edge. It resolves against the
  producer's current receipt: the receipt must match the producer's current
  desired unit. Missing or stale evidence blocks the consumer; invalid evidence
  fails validation.
- Only outputs listed in `outputs` are persisted. Drivers reject outputs marked
  sensitive by the native tool. Credentials and private keys never enter
  desired state, receipts, public artifacts, or diagnostic transcripts.
- When upstream outputs are unavailable, retain the unresolved desired intent
  and resolve dependents as evidence arrives, without rereading unrelated
  mutable source. Each run is bounded to one source revision; the contract
  defines how desired revisions advance within that run.
- Artifacts (image digests, rendered trees) are recorded as immutable
  descriptors with integrity digests; large payloads stay outside Git.
- The Kubernetes publication unit wraps `render-tree` and `publish-tree` for one
  DeploymentTarget, because one target owns one publication tree and ownership
  index. Its desired unit contains the resolved Release input snapshot, keyed by
  `<applicationGroup>/<release>`; its recorded result is the published commit
  and index digest.
  Publication, Argo CD acceptance, and observed health are distinct evidence
  levels; a publication-only result cannot satisfy a dependency that requires a
  healthy deployment.

**Command unit (M3).** A constrained escape hatch for tools without a driver:

```yaml
spec:
  driver: Command
  inputs:
    files: ["scripts/seed/**"]
    values: {bucket: {fromUnit: {unit: storage, output: /bucket}}}
  command: ["./scripts/seed/run.sh"]
  outputs: [seedVersion]
```

The command runs in a checkout at the pinned source revision and receives
resolved values in a JSON file whose path is passed in the environment. It writes
its outputs to a second JSON file; stdout and stderr are the transcript and are
never parsed. Only declared outputs are recorded, and outputs declared
`sensitive` are validated but never persisted. Nyl requires but cannot enforce
idempotency for identical inputs, so the contract documents it as the author's
obligation and treats an interrupted run as uncertain completion. An optional
`verify` command exits with a documented clean, drift, or error status and
records no receipt. The environment, secret admission, and sandboxing are M1
decisions. Command units exist to learn which typed drivers are worth building.

**Pinned source trees.** Units that execute repository content (Terraform, image
contexts, commands) record the exact source commit and path in their desired
unit and execute from a worktree at that commit. Relative Terraform modules are
resolved inside that checkout, so pinning the root configuration pins its local
modules too; a module that needs independent versioning is a separate unit or a
versioned remote module. The input fingerprint covers the selected bytes,
including `.terraform.lock.hcl`, not the commit value, so re-pinning to
identical inputs plans no change. A pinned commit must remain fetchable: require
it to be reachable from a protected ref, or retain it with a Nyl-owned keep ref.

### Promotion paths (M6)

A PromotionPath is the explicit link between source selectors and target
bindings. Source and target may differ in unit names, input names, and document
shape; the path states the mapping. Names and fields below are proposals.

```yaml
apiVersion: orchestration.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-staging}
spec:
  from: {environment: dev}
  to: {environment: staging}
  evidence: healthy            # publication | accepted | healthy, per driver
  changeGate: pullRequest      # or: none
  values:
    webImage:
      select: {unit: kubernetes, input: /releases/platform~1web/image}
    databaseSource:
      select: {unit: database, input: /source}
```

Target consumers name the path and value, never the source unit:

```yaml
# staging DeploymentTarget
releaseInputs:
  platform/web:
    image: {fromPromotion: {path: dev-to-staging, value: webImage}}
---
# staging Terraform unit
inputs:
  source: {fromPromotion: {path: dev-to-staging, value: databaseSource}}
```

- `select` reads either a source unit's **resolved input** from its desired unit
  (what the source environment actually ran, such as the deployed image digest
  or the applied Terraform source commit) or its recorded **output**.
- All values of one promotion come from one consistent source snapshot: a single
  source desired revision and a single observed revision. The default is the
  newest desired revision at which every selected unit has a matching receipt at
  the required evidence level; `--from-revision` selects an exact one. Values
  from different source revisions were never run together and are not combined.
- Evidence levels are those the source driver records in receipts. For the
  Kubernetes publication unit, publication alone does not prove a deployment;
  promotion from it normally requires acceptance or health evidence. Drift
  verification writes no receipt and is therefore not a promotion evidence level.
- `nyl orchestrate promote --path dev-to-staging [--value …]` writes a
  PromotionRecord into the target environment's desired state: each value, its
  selector, the source unit's identity and incarnation, both pinned source
  revisions, and the receipt digest. With `changeGate: pullRequest` it opens a
  reviewable change instead.
- Promoting a path moves all of its values together by default. Selecting a
  subset is explicit and leaves the other values at their previously recorded
  lineage.
- `plan` validates every path: missing source units, unresolvable pointers, and
  type mismatches with the target binding are reported before promotion.
  Renaming or replacing a source unit breaks future promotions until the path is
  updated; recorded lineage keeps the old identity and stays valid.
- A binding to a path that has not been promoted yet blocks with an actionable
  message. A broken selector in an existing path is an error, not a wait.
- Native state is never promoted; the staging Terraform unit applies the
  promoted source commit and variables against staging's own backend.

Because promotion records live in Git, a target that does not use orchestration
can still consume a promoted value through a locked `fromGit` binding once the
state layout is fixed in M3.

### State, evidence, and recovery

| State | Authority |
| --- | --- |
| Authored intent | Source Git |
| Resolved desired units and promotion records | Desired-state Git ref per environment |
| Receipts, public outputs, artifact descriptors | Observed-state Git ref per environment |
| Terraform resource state | Terraform/OpenTofu backend |
| Credentials and private keys | Secret store or execution environment |
| Render cache | Disposable local storage |
| Execution coordination | Explicit claims/leases and recovery records |

Desired and observed refs are distinct so desired state can advance while each
receipt continues to identify the exact desired unit it observed. Preserve the
distinction between rendered-file ownership, published desired state,
successful execution evidence, and current external observations. A matching
receipt proves success for particular inputs; it does not prove the system is
still healthy or free of drift. Rollback publishes a new forward desired
revision; it never rewinds a ref or observed history.

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
- Ordering: creation dependencies do not automatically define safe replacement
  or decommissioning order. Unsupported teardown remains visibly blocked.

### Kubernetes rendering path

Clusters can explicitly borrow CRD schemas or the complete Kubernetes API
contract from another declared Cluster. Effective capabilities drive both
rendering and validation; destinations, values, and live connection settings
remain local. `nyl capture cluster` refreshes committed capabilities and
CRD schema snapshots by default, with capabilities-only overrides. Project-configured
validators check final artifacts. Complete tree validation uses desired CRDs for
rendered resources by default, with an invocation opt-out; compatibility outside
the rendered input and upgrade ordering require separate handling. Builtin schema
policy selects disposable caching, vendoring of used schemas, or complete pinned
Kubernetes version directories with offline inventory verification.
Rendering emits inspectable artifacts even when validation fails and returns a
failing exit status; `render-tree --check` writes no output. Validation gates diff
calculation, application, and publication. These operations remain independently
usable without orchestration state. Validation findings and text/JSON exports
share structured resource results, schema origins, and authoring provenance.
Reports retain completed results on operational failure and distinguish invalid
resources from unchecked inputs; rendering caches retain the provenance needed
for identical reports on cache hits.

Kubernetes normalization, namespace handling, deduplication, and policy
processing belong to this path. Every input affecting rendered bytes, including
resolved Release inputs, participates in dependency recording; cache entries are
disposable and never establish execution success.

Kubernetes delivery distinguishes publication for an external reconciler,
publication plus observation, and direct application through Nyl. Observation of
Argo CD does not authorize Nyl to apply or prune its workloads. Reusing Nyl's
direct Kubernetes lifecycle inside orchestration requires explicit compatibility
decisions about release history, pruning, and readiness.

## User and machine interfaces

Preserve `nyl render`, `diff`, `apply`, `render-tree`, and `publish-tree` with their
Kubernetes meanings. Release inputs extend them without new required flags.

`nyl comment upsert` publishes supplied Markdown as a sticky PR/MR comment on
GitHub, GitLab, or Forgejo. Report generation remains separate from posting;
comment keys and authenticated account ownership require no orchestration state
or Nyl project configuration.

The working CLI design uses an explicit `nyl orchestrate` group. Names and flags
are proposals to validate in M1/M3:

```bash
nyl update source-locks --check
nyl orchestrate plan --environment dev
nyl orchestrate reconcile --environment dev
nyl orchestrate status --environment dev
nyl orchestrate verify --environment dev
nyl orchestrate promote --path dev-to-staging
nyl orchestrate delete --environment dev --unit web-image
```

| Operation | Contract |
| --- | --- |
| plan | Preview effects and unresolved inputs without executing |
| reconcile | Resolve source into desired state and drive ready units through dependency waves |
| status / get | Inspect intent, evidence, progress, promotion lineage, and blockers |
| verify | Observe external state and report drift without writing receipts |
| promote | Record selected source values into target desired state, or open a change for review |
| delete | Record explicit deletion intent for reconciliation |

Avoid a second meaning for `apply`. Specify command effects, selection defaults,
non-interactive behavior, deadlines, and exit categories before stabilizing the
CLI.

A plan with unavailable upstream outputs is incomplete and must say so. Preview
values cannot become execution inputs. Approval policy must define whether newly
resolved downstream plans execute automatically or require further review. Do
not promise execution of an approved native plan unless the driver preserves and
validates that exact plan and its input/state preconditions.

Inspection explains desired revision, matching or stale evidence, the precise
dependency or promotion blocking progress, execution owner, and when external
state was last verified. Do not collapse recorded success and current health
into one status. Human tables and machine output derive from the same snapshot.
Keep machine output on stdout and human diagnostics on stderr, with versioned
structured results.

## Milestones and acceptance criteria

### M1 — Unit, input, state, and promotion contract

- [ ] Define Release input declarations, DeploymentTarget bindings, types, and
  lock semantics for `fromGit`.
- [ ] Define Unit, Environment, and PromotionPath schemas; typed references;
  output admission; and artifact descriptors.
- [ ] Define desired/observed ref layout, receipt freshness, identity fences,
  leases, deletion, and recovery invariants.
- [ ] Define the driver trait, capabilities, and the command unit's process
  contract.
- [ ] Specify CLI effects, revision selection, partial plans, and exit
  categories.

**Exit criterion:** the contract explains a successful dependency wave, an
unavailable upstream output, effects without a receipt, competing runners, a
promotion with stale source evidence, and deletion, without hidden state or
ambiguous command semantics.

### M2 — Release inputs without orchestration

- [ ] Implement Release inputs and DeploymentTarget bindings for `value`,
  `fromFile`, `fromGit`, and `fromPublication`, with type validation and
  defaults.
- [ ] Support `carry` for `fromPublication`, excluding carried files from the
  dirty check.
- [ ] Extend `nyl update source-locks` to refresh `fromGit` locks, with a
  `--target` filter.
- [ ] Apply target bindings in direct commands and add `--input`/`--inputs`.
- [ ] Record resolved inputs in the dependency recorder, render-cache key, and
  the existing ownership-index `inputs` map as digests.
- [ ] Reject `fromUnit` and `fromPromotion` bindings outside orchestration with
  an actionable message.
- [ ] Document the feature and regenerate resource schemas.

**Exit criterion:** a target renders Releases from static, locked external, and
same-branch publication state inputs, committed or carried, through
`render-tree` and `publish-tree`;
a concurrent state push makes publication fail rather than interleave; projects without inputs produce
byte-identical output to the previous release.

### M3 — Orchestration core with a constrained command unit

- [ ] Implement environments, desired/observed refs, receipts, and leases.
- [ ] Implement `plan`, `reconcile`, `status`, and `verify` for a dependency
  graph of command units with `fromUnit` references.
- [ ] Implement pinned source worktrees and content-based input fingerprints.
- [ ] Prove stale-evidence blocking, competing runners, and recovery after an
  effect succeeds but receipt publication fails.

**Exit criterion:** two dependent command units reconcile locally and in CI with
identical results; repeat execution is a no-op; interruption has a demonstrated
recovery path.

### M4 — Container image and Terraform units

- [ ] Add an image-build unit recording immutable digest references.
- [ ] Add a Terraform/OpenTofu unit with native state and locking, declared
  output admission, planning, and verification.
- [ ] Support a local-module Terraform configuration pinned to a source commit
  and path.
- [ ] Demonstrate Terraform-to-Terraform output references.

**Exit criterion:** a network configuration's outputs feed a dependent
configuration; an image build records a digest; unchanged inputs plan no change.

### M5 — Images and Terraform outputs into Kubernetes releases

- [ ] Add the Kubernetes publication unit over `render-tree`/`publish-tree`.
- [ ] Resolve `fromUnit` Release input bindings into an explicit pinned input
  snapshot for rendering.
- [ ] Distinguish publication, acceptance, and health evidence for dependents.
- [ ] Document an end-to-end local/CI example.

**Exit criterion:** one reconcile builds an image, applies Terraform, and
publishes Kubernetes manifests consuming both; the same Releases still render
with static inputs in a target that does not use orchestration.

### M6 — Promotion paths

- [ ] Implement PromotionPath, PromotionRecord, and `nyl orchestrate promote`
  with evidence checks and an optional pull-request change gate.
- [ ] Promote an image digest and a Terraform source commit from dev to staging
  across differently named units and inputs, from one consistent snapshot.
- [ ] Show promotion lineage in `status` and consume a promoted value through
  `fromGit` in a non-orchestrated target.

**Exit criterion:** staging runs exactly the image digest and Terraform source
dev proved, with auditable lineage; stale or missing source evidence blocks
promotion.

### M7 — Continuous operation and scope decision

- [ ] Evaluate a scheduled or long-running runner, observation cadence, and
  drift-repair policy using M3–M6 evidence.
- [ ] Decide which command-unit uses warrant typed drivers.
- [ ] Record the selected direction and constraints in this roadmap.

## Open decisions

Resolve these at the milestone where they affect implementation; they are not
reasons to delay independent work.

| Decision | Needed by |
| --- | --- |
| Environment declaration and state ref configuration | M1 |
| Per-driver evidence levels and their names | M1/M5 |
| Desired, observed, and coordination ref names and authorization | M1 |
| Command unit sandboxing, environment variables, and secret admission | M1/M3 |
| Terraform versus OpenTofu executable support and plan approval semantics | M4 |
| Image build backend (BuildKit, Docker, Buildah) and registry authentication | M4 |
| Automatic downstream execution policy after new evidence | M3/M5 |
| Promotion record location for pull-request gates | M6 |
| Continuous runner ownership, observation cadence, and drift-repair policy | M7 |

## Implementation reference points

- [Nyl rendering session and bundle](nyl/src/render/session.rs)
- [Nyl components](nyl/src/components/mod.rs)
- [Kubernetes GitOps resource model](nyl/src/resources/gitops.rs)
- [Source-lock updates](nyl/src/cli/commands/source.rs)
- [Rendered-file ownership reconciliation](nyl/src/gitops/reconcile.rs)
- [Rendered-tree publication](nyl/src/cli/commands/publish_tree.rs)
- [Git worktrees](nyl/src/git/worktree.rs)
- [Direct Kubernetes application](nyl/src/cli/commands/apply.rs)
- [Kubernetes release state](nyl/src/kubernetes/state.rs)

These are navigation aids, not frozen API guarantees.
