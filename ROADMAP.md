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

**Next step:** review the draft [orchestration core contract](design/orchestration-core.md)
(M3) and [infrastructure units contract](design/infrastructure-units.md) (M4).
The [Release inputs contract](design/release-inputs.md) settles M2's design, so
M2 implementation can start independently.

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
capabilities (plan, reconcile, verify, inspect, teardown). The command unit is
the extension point for tools without a dedicated driver. Every type crossing
the trait is serializable, so plugin drivers (executables with their own unit
API group, speaking a versioned JSON protocol) remain possible; the protocol
itself is an M7 decision, not part of the initial scope.

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

Orchestration is still GitOps, so its resources join the existing groups.
Environment and PromotionPath use `gitops.nyl/v1`, next to the shared
GitRepository, which also names environment state repositories. Built-in unit
kinds (`Command`, `Terraform`, `OpenTofu`, `OciImage`, `KubernetesPublication`) use
`units.gitops.nyl/v1`, where every kind is a unit and the kind selects the
driver, as in `components.k8s.nyl/v1`. Kubernetes compiler resources keep
`k8s.gitops.nyl/v1`; HelmChart and RemoteManifest keep `k8s.nyl/v1`. Resource reference pages derive
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
| `fromUnit` | Recorded public output or published artifact field of a unit in the same environment | Yes (M5) |
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
  digest. The path is relative to the target's publication prefix and must stay
  outside Argo CD-synced directories. With `carry`,
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

The draft [orchestration core contract](design/orchestration-core.md) specifies
environments, units, state, execution, recovery, deletion, drivers, and the
command unit. In summary, an Environment selects labelled unit templates of any
unit kind and renders them with its values, mirroring how a DeploymentTarget selects
ApplicationGroups, so a unit is written once for every environment:

```yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: dev}
spec:
  unitSelector: {matchLabels: {tier: platform}}
  values: {stateKey: dev/database, target: dev}
---
apiVersion: units.gitops.nyl/v1
kind: Terraform
metadata: {name: database, labels: {tier: platform}}
spec:
  source: {path: infra/database}
  backend: {key: '{{ values.stateKey }}'}
  variables:
    vpcId: {fromUnit: {unit: network, output: vpcId}}
  outputs:
    host: {type: string}
    port: {type: integer}
---
apiVersion: units.gitops.nyl/v1
kind: KubernetesPublication
metadata: {name: kubernetes, labels: {tier: platform}}
spec:
  target: '{{ values.target }}'   # DeploymentTarget whose Release inputs use fromUnit
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
- Only outputs declared in `outputs` are persisted, typed with the Release
  input type set. Outputs declared `sensitive` are validated but never
  persisted or referenceable; undeclared outputs are ignored, and a declared
  output the native tool marks sensitive must be declared sensitive. Credentials and private keys never enter
  desired state, receipts, public artifacts, or diagnostic transcripts.
- When upstream outputs are unavailable, retain the unresolved desired intent
  and resolve dependents as evidence arrives, without rereading unrelated
  mutable source. Each run is bounded to one source revision; the contract
  defines how desired revisions advance within that run.
- Artifacts are typed documents in `artifacts.gitops.nyl/v1`, such as
  `ContainerImage` and `PublishedTree`, checked against digests recorded in the
  receipt and referenced with `fromUnit: {unit, artifact, pointer}`; large
  payloads stay outside Git.
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
apiVersion: units.gitops.nyl/v1
kind: Command
metadata: {name: seed}
spec:
  files: ["scripts/seed/**"]
  values: {bucket: {fromUnit: {unit: storage, output: bucket}}}
  command: ["./scripts/seed/run.sh"]
  env:
    passthrough: [AWS_REGION]
    secrets: {DB_PASSWORD: database-password}
  outputs:
    seedVersion: {type: string}
```

The command runs in a checkout at the pinned source revision and receives
resolved values in a JSON file whose path is passed in the environment. It writes
its outputs to a second JSON file; stdout and stderr are the transcript and are
never parsed. Only declared outputs are recorded, and outputs declared
`sensitive` are validated but never persisted. Nyl requires but cannot enforce
idempotency for identical inputs, so the contract documents it as the author's
obligation and treats an interrupted run as uncertain completion. An optional
`verify` command exits with a documented clean, drift, or error status and
records no receipt. The process starts from an empty environment plus declared
runner variables and secrets from the project's secrets provider, which are
masked in transcripts; there is no further sandbox. An interrupted command is
re-run only when it declares `idempotent: true`, otherwise it waits for an
operator. Command units exist to learn which typed drivers are worth building.

**Pinned source trees.** Units that execute repository content (Terraform, image
contexts, commands) record the exact source commit and path in their desired
unit and execute from a worktree at that commit. Relative Terraform modules are
resolved inside that checkout, so pinning the root configuration pins its local
modules too; a module that needs independent versioning is a separate unit or a
versioned remote module. The input fingerprint covers the selected bytes,
including `.terraform.lock.hcl`, not the commit value, so re-pinning to
identical inputs plans no change. A pinned commit must remain fetchable, so it
must be reachable from a protected ref or its lock's own branch; run source
commits must be reachable too, unless an environment opts into unprotected
sources.

### Promotion paths (M6)

A PromotionPath is the explicit link between source selectors and target
bindings. Source and target may differ in unit names, input names, and document
shape; the path states the mapping. Names and fields below are proposals.

```yaml
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-staging}
spec:
  from: {environment: dev}
  to: {environment: staging}
  evidence: healthy            # published | accepted | healthy
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
# staging Terraform unit (units.gitops.nyl/v1 Terraform)
spec:
  source: {fromPromotion: {path: dev-to-staging, value: databaseSource}}
```

- `select` reads either a source unit's **resolved input** from its desired unit
  (what the source environment actually ran, such as the deployed image digest
  or the applied Terraform source commit), a recorded **output**, or a field of
  a published **artifact**.
- Evidence levels are one list for every source:
  - `published`: the source produced the value, as a receipt or, for a target
    source, a publication commit.
  - `accepted`: the newest deployment of each covered Application is at a
    revision whose Application directory tree matches the publication that
    carries the value.
  - `healthy`: accepted, and each covered Application is `Healthy`.

  `accepted` and `healthy` are defined by Argo CD for Kubernetes publications.
  Units without a deployment observation, such as Terraform, image builds, and
  commands, satisfy every level with a current receipt, and the revision such a
  unit "runs" is that of its current receipt; a driver may record stronger
  evidence, such as post-apply checks, in its receipt. Drift verification
  writes no receipt and is not an evidence level.
- One rule selects values: promote, per value, the newest source revision whose
  evidence proves the required level. At `accepted` and `healthy`, that is the
  revision each value's consumer runs now, so `promote` first takes a fresh
  observation of the covered Applications. The observation is always stored in
  the PromotionRecord; environment sources also record it in observed state.
- All values of one promotion come from one consistent source state. At
  `published`, that is a single source revision: the newest at which every
  selected unit has a matching receipt or, for a target source, the newest
  publication commit; `--from-revision` selects an exact one. At `accepted` and
  `healthy`, `--from-revision` is rejected, and the state is what the source
  runs at the moment of the observation: each
  value comes from the revision its own consuming Application or unit runs, so
  values proven together in the source are promoted together even when manual
  syncs left Applications at different revisions.
- `nyl promote dev-to-staging [--value …]` writes a
  PromotionRecord into the target environment's desired state: per value, its
  selector, the source unit's identity and incarnation, the source revision it
  came from, and its receipt or input digest; plus the observation that proved
  the set at `accepted` or `healthy`. With `changeGate: pullRequest` it opens a
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

**Promotion sources.** `from` selects either an environment, as above, or a
DeploymentTarget. A target source lets a dev target that renders from `value`,
`fromGit`, or `fromPublication` bindings (including `carry`) feed an
orchestrated environment without being orchestrated itself:

```yaml
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-production}
spec:
  from: {target: dev}
  to: {environment: production}
  evidence: published
  changeGate: pullRequest
  values:
    webImage:
      select: {input: platform/web/image}   # <group>/<release>/<input>
```

- A target's publication commits are the commits that change
  `<prefix>/_nyl/index.json`. Commits by other targets on a shared branch and
  external state write-backs are never value sources.
- At `published`, a target source reads one publication commit: the newest by
  default, or an exact one with `--from-revision`. That commit already contains
  the manifests rendered from the values. At `accepted` and `healthy`, each
  value comes from the publication its consuming Application runs, as described
  under health evidence.
- The ownership index stores input digests, not values. Promotion recovers each
  value from its recorded provenance: the carried or base-commit state file, the
  locked `fromGit` blob, or the binding at the recorded source commit. It then
  verifies the value against the recorded `@input` digest. A publication made
  from a dirty source worktree is not a promotion source.
- A non-orchestrated target records no observations of its own, so `accepted`
  and `healthy` come from the fresh observation taken when promoting.
- The PromotionRecord adds, per value, the source target, publication
  repository, branch, and commit, and the input digest to its lineage, plus the
  observation that proved the set.
- `to` is always an environment, because the PromotionRecord lives in that
  environment's desired state.

**Promotion without orchestration.** A target that belongs to no environment
promotes through a locked `fromGit` binding to the source target's published
state instead: `nyl update source-locks --target production` moves the lock to
the newest source publication (with `--require healthy`, to the publication the
consuming source Application runs), and the pull request that commits the lock is the
review. The two routes coexist:

| | Locked `fromGit` (M2) | PromotionPath (M6) |
| --- | --- | --- |
| Target binding | `fromGit` to a source publication commit | `fromPromotion` naming a path and value |
| Promote with | `nyl update source-locks --target …`, then a pull request | `nyl promote <path>` |
| Health gate | `--require healthy` on the lock update | `evidence: healthy` on the path |
| Record | The lock in source, with an `observed` block under `--require healthy` | A PromotionRecord in target desired state |
| Adds | — | Evidence gates, atomic multi-value promotion, lineage |

### Health evidence

Promoting only what is healthy in the source requires observing Argo CD. Nyl
knows the Applications it generates for a target, their ArgoCDInstance, and its
Cluster.

- **Observer.** Nyl reads the target's generated Applications from the Argo CD
  control-plane Cluster through its local context. Health checks need
  credentials for that Cluster, which publication does not.
- **Running revision.** Argo CD's Synced/OutOfSync status compares against the
  branch head, so an Application still running an older commit reports
  OutOfSync as soon as a newer publication changes its files. Nyl ignores it.
  The running revision is that of the newest `status.history` entry, which
  Argo CD appends only for full, non-dry-run syncs. The sync result is not
  used, because Argo CD also records selective and dry-run syncs there as
  succeeded. A selective sync newer than that entry leaves the Application
  running a mix of revisions, which proves no revision. Health is Argo CD's
  live health.
- **Matching a publication.** The running revision R may be another target's
  commit or a state write-back on a shared branch. For each covered
  Application, Nyl finds the source target's publication commits (commits that
  change `<prefix>/_nyl/index.json`) that are R or an ancestor of it, and whose
  Application directory tree and recorded inputs for that Release equal those
  at R. Values are always recovered from a matching publication commit, never
  from R.
- **Recorded commit.** The running publication is what gets promoted. Among the
  publication commits equivalent to it, the PromotionRecord (or the lock's
  `commit`) names the oldest in the unbroken run of matching commits ending at
  R: the commit that introduced what runs now. A change that was later reverted
  starts a new run, so a tree that went A → B → A records the second A. When no Application changed between the publications that
  different Applications run, every value names the same commit, so the audit
  trail stays a single commit; otherwise each value names the commit that last
  changed its own Application.
- **Coverage.** By default, the Applications whose Releases produced the
  promoted values must be accepted or healthy. A PromotionPath may add
  Applications that must be healthy at whatever publication they run, without
  contributing values.
- **Manual syncs.** Applications synced to different publications do not
  block promotion. Promotion blocks only when a covered Application runs a
  revision that matches no source publication, or does not meet the level.
- **Decision evidence is always recorded.** The observation behind every
  promotion (time, Application, running revision, health) is stored in the
  PromotionRecord, additionally in observed state for environment sources, and
  in the lock's `observed` block for the lock route.
- **Observation history.** One observation proves what runs now. Promoting a
  commit that no longer runs, requiring a minimum healthy duration, and
  guarding against Applications that flap between healthy and unhealthy need
  observations over time; Argo CD does not reliably report how long an
  Application has been healthy. The publication unit's observe mode records
  them in an environment's observed state; periodic observation is part of
  continuous operation.
- **Meaning.** Argo CD health means Kubernetes considers the resources ready,
  such as a completed Deployment rollout. It does not prove the application
  works. Application-level checks, such as HTTP probes or smoke tests, can be
  added later as command units whose recorded results a PromotionPath also
  requires.

### State, evidence, and recovery

| State | Authority |
| --- | --- |
| Authored intent | Source Git |
| Desired units (with deletion and hold lifecycle) and promotion records | Desired-state Git ref per environment |
| Latest receipt and condition per unit, artifacts, latest observations | Observed-state Git ref per environment |
| Terraform resource state | Terraform/OpenTofu backend |
| Credentials and private keys | Secret store or execution environment |
| Render cache | Disposable local storage |
| Execution coordination | A per-environment lease ref and disposable per-run checkpoint refs, outside the desired and observed refs |

Each Environment names a state repository (the source repository by default)
and a desired and an observed ref. They may be the same ref; the layout uses
fixed `desired/` and `observed/` directories either way. Desired state is
derived from source and evidence, so the reconcile runner writes it; review
happens on source changes and on promotions, whose pull-request gate targets
the desired ref. Separate refs keep that review and branch protection apart
from the frequent observed commits. Each receipt identifies the execution key
of the desired unit it executed. To keep the state refs reviewable, each
operation writes at most one transition commit per ref: a whole reconcile is
one commit, whose message carries a machine-readable summary and trailers
(operation, run ID, source commit, the state commits read, runner, evaluation
time, Nyl version), so history can be audited and replayed by machines.
Per-unit progress and crash recovery use the disposable run refs. Preserve the
distinction between rendered-file ownership, published desired state,
successful execution evidence, and current external observations. A matching
receipt proves success for particular inputs; it does not prove the system is
still healthy or free of drift. Rollback publishes a new forward desired
revision; it never rewinds a ref or observed history.

The lifecycle contract must address:

- Effects that succeed before receipt publication fails: inspect or resume
  safely; missing evidence does not imply that nothing happened.
- Competing or stale runners: Git compare-and-swap protects publication, while
  execution leases and native locks protect effects. A run's final state
  commit is pushed atomically with a check of its lease, so a run that lost
  its lease writes no state; its checkpointed results are imported by the next
  run.
- Timeouts and disconnected processes: represent uncertain completion and
  recover before blindly repeating an operation.
- Partial input: omission requests deletion only within a declared authoritative
  ownership set; selecting a subset is not an instruction to destroy the rest.
- Deletion: retain sufficient desired inputs and evidence for retry-safe teardown
  and fence it against a new incarnation of the same name.
- Ordering: creation dependencies do not automatically define safe replacement
  or decommissioning order. Units are torn down before the units they depend
  on, and the units a Kubernetes publication depends on also wait until its
  workloads are gone, per the publication's `teardownWait` strategy (observed
  removal, a fixed delay, or operator confirmation).
- Deletion defaults: `deletionPolicy` defaults to `Teardown` where a kind
  supports it, gated by `--allow-teardown` for omissions, so an accidental
  omission destroys nothing and is undone by restoring the unit. A declared
  `Retain` keeps a tombstone, so restoring a retained unit never re-runs it.

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

Orchestration commands are top-level, because it is all GitOps. Names and flags
are proposals to validate in M3:

```bash
nyl update source-locks --check
nyl plan -e dev
nyl reconcile -e dev [--unit …] [--approve …] [--allow-teardown]
nyl status -e dev
nyl verify -e dev
nyl promote dev-to-staging
nyl teardown -e dev --unit web-image [--hold]
nyl hold -e dev --unit web-image --reason "incident 4711"
nyl resume -e dev --unit web-image
nyl recover -e dev --unit seed --retry
nyl state forget -e dev --unit worker   # drop a retained tombstone, resources untouched
nyl get units                        # declarations
nyl get units -e dev                 # state of an environment
nyl get output network/vpcId -e dev
nyl get artifact web-image/image -e dev --pointer /reference
nyl get promotions -e staging
nyl delete unit web-image            # removes the declaration from source
```

`nyl get` reads source without `-e` and an environment's state with it; `-o
yaml` returns exact persisted documents, and `get output` and `get artifact
--pointer` return single values that resolve exactly like `fromUnit`
references, failing when those would.

`nyl release` (Kubernetes release history) and `nyl delete` (source editing)
keep their meanings. `nyl create` and `nyl delete` gain environments, units,
and promotion paths; `nyl get` covers those declarations and, with `-e`, an
environment's units, outputs, artifacts, and promotions. Deleting a unit
declaration is the GitOps way to remove it, handled by the next `reconcile` under its deletion
policy.

| Operation | Contract |
| --- | --- |
| plan | Preview effects and unresolved inputs without executing |
| reconcile | Resolve source into desired state and drive ready units through dependency waves |
| status | Inspect intent, evidence, progress, promotion lineage, and blockers |
| verify | Observe external state and report drift without writing receipts |
| promote | Record selected source values into target desired state, or open a change for review |
| teardown | Tear down a unit: complete a deletion, replace a selected unit, or with `--hold` keep it down |
| hold / resume | Freeze a unit so no changes to it are reconciled, without touching its resources; resume lifts the freeze |
| recover | Clear an uncertain condition or non-retryable failure for re-execution, with a recorded reason |
| state init / move / forget | Create state, relocate it and retire the old location, or drop a unit from state without touching its resources |

Avoid a second meaning for `apply`. Specify command effects, selection defaults,
non-interactive behavior, deadlines, and exit categories before stabilizing the
CLI.

A plan with unavailable upstream outputs is incomplete and must say so. Preview
values cannot become execution inputs. `reconcile` runs newly unblocked
dependents in further waves of the same run; `--unit`/`--units` restricts
execution, and units with `approval: manual` run only when named with
`--approve`. Teardown caused by omission requires `--allow-teardown`. Exit
categories distinguish converged, error, blocked, failed, and uncertain. Do
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

- [x] Define Release input declarations, DeploymentTarget bindings, types, and
  lock semantics for `fromGit` ([contract](design/release-inputs.md)).
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
- [ ] Apply target bindings in direct commands and add `--input`/`--inputs`;
  a target that selects no group containing the Release fails unless
  `--application-group` or `--defaults-only` is given.
- [ ] Require unique generated Argo CD names across every pair of targets whose
  instances resolve to the same cluster and namespace, including implicit
  per-target instances.
- [ ] Record resolved inputs in the dependency recorder, render-cache key, and
  the existing ownership-index `inputs` map as digests under reserved
  `@`-prefixed keys, with an index format version and migration.
- [ ] Reject `fromUnit` and `fromPromotion` bindings outside orchestration with
  an actionable message.
- [ ] Document the feature and regenerate resource schemas.
- [ ] Extract `nyl-core` and `nyl-render` from the current crate without
  behavior change, per the implementation architecture.

**Exit criterion:** a target renders Releases from static, locked external, and
same-branch publication state inputs, committed or carried, through
`render-tree` and `publish-tree`;
a concurrent state push makes publication fail rather than interleave; projects without inputs produce
byte-identical output to the previous release.

### M3 — Orchestration core with a constrained command unit

- [ ] Implement environments, YAML state files with published schemas,
  `nyl state init`/`move`/`forget`/`delete` (including decommissioning declared
  environments), leases and run checkpoints, and one
  transition commit per operation with a machine-readable summary, pushed
  atomically with the lease check.
- [ ] Support SSH keys and HTTPS tokens, besides the SSH agent, for state
  pushes and `publish-tree`.
- [ ] Implement `plan`, `reconcile`, `status`, `verify`, `recover`, `teardown`
  (including `--hold`), `hold`, and `resume` for a dependency graph of command
  units with `fromUnit` references, including `--local` runs with and without
  remote state.
- [ ] Implement approvals bound to the desired document, with recorded approval
  sources, and the common `env` credential admission.
- [ ] Implement pinned source worktrees, reachability checks against protected
  refs, and content-based execution keys.
- [ ] Prove stale-evidence blocking, competing runners, and recovery after an
  effect succeeds but receipt publication fails.
- [ ] Start `nyl-state`, `nyl-orchestration`, and `nyl-drivers` as separate
  crates with the scenario harness, property tests, and crash injection.
- [ ] Add the `test-clock` Cargo feature and the two-tier harness for the
  reference scenarios: in-process with fake tool drivers, and through the real
  binary where the tools are installed.

**Exit criterion:** two dependent command units reconcile locally and in CI with
identical results; repeat execution is a no-op; interruption has a demonstrated
recovery path; the M3 part of the
[reference scenarios](design/reference-scenarios.md) passes.

### M4 — Container image and Terraform units

- [ ] Add the `OciImage` kind over `docker buildx`, recording digest references
  and a `ContainerImage` artifact, with the `registryAuth` helper.
- [ ] Add the `Terraform` and `OpenTofu` kinds over one implementation, with
  native state and locking, declared output admission, change digests, plan
  approval (`bind: plan`), verification, and teardown.
- [ ] Support a local-module Terraform configuration pinned to a source commit
  and path.
- [ ] Demonstrate Terraform-to-Terraform output references.

**Exit criterion:** a network configuration's outputs feed a dependent
configuration; an image build records a digest; unchanged inputs plan no change;
the M4 part of the reference scenarios passes with the real tools.

### M5 — Images and Terraform outputs into Kubernetes releases

- [ ] Add the Kubernetes publication unit over `render-tree`/`publish-tree`.
- [ ] Resolve `fromUnit` Release input bindings into an explicit pinned input
  snapshot for rendering.
- [ ] Distinguish `published`, `accepted`, and `healthy` evidence for dependents.
- [ ] Add the publication unit's observe mode: read generated Argo CD
  Applications' last successful sync and health, match them to publication
  commits by Application directory tree, and record the observations.
- [ ] Document an end-to-end local/CI example.
- [ ] Support inline DeploymentTargets on `KubernetesPublication`, its
  two-phase teardown with the `teardownWait` strategies, ownership-index
  owner fencing, and the teardown readiness check with its warnings.
- [ ] Add EnvironmentTemplates for preview environments: instances through
  `nyl state init --template`, `teardown --all`, `state delete`, shared state
  refs through `state.path`, cross-environment references to declared
  environments, sliding expiry, and `maxInstances`.

**Exit criterion:** one reconcile builds an image, applies Terraform, and
publishes Kubernetes manifests consuming both; the same Releases still render
with static inputs in a target that does not use orchestration; all three
reference scenarios, including preview closure and expiry, pass in both tiers.

### M6 — Promotion paths

- [ ] Implement PromotionPath, PromotionRecord, and `nyl promote`
  with evidence checks and an optional pull-request change gate.
- [ ] Promote an image digest and a Terraform source commit from dev to staging
  across differently named units and inputs, from one consistent source state.
- [ ] Promote from a non-orchestrated target's published inputs, including a
  carried state file, with values verified against the recorded digests.
- [ ] Gate promotion on a fresh Argo CD observation for both PromotionPath
  sources, sourcing each value from the publication its Application runs, and
  add `nyl update source-locks --require healthy` with an `observed` block.
- [ ] Show promotion lineage in `status`.

**Exit criterion:** staging runs exactly the image digest and Terraform source
dev proved, with auditable lineage; stale or missing source evidence blocks
promotion.

### M7 — Continuous operation and scope decision

- [ ] Evaluate a scheduled or long-running runner, observation cadence, and
  drift-repair policy using M3–M6 evidence.
- [ ] Decide which command-unit uses warrant typed drivers, and whether to add
  the plugin driver protocol.
- [ ] Support minimum healthy durations, flapping guards, and promotion of
  commits that no longer run, from recorded observations.
- [ ] Evaluate application-level checks as promotion evidence.
- [ ] Record the selected direction and constraints in this roadmap.

## Open decisions

Resolve these at the milestone where they affect implementation; they are not
reasons to delay independent work.

| Decision | Needed by |
| --- | --- |
| Argo CD control-plane credentials for health checks in CI | M5/M6 |
| Command unit isolation beyond the declared environment | M7 |
| Plugin driver protocol, registration, and pinning | M7 |
| Approver lookup for CI systems other than GitHub | M3 |
| Additional image build backends and registry-specific image deletion | After M4 |
| Continuous runner ownership, observation cadence, and drift-repair policy | M7 |

## Implementation reference points

- [Implementation architecture](design/implementation-architecture.md): crate
  boundaries, pure decisions, rule ownership, and scenario tests for M2–M7
- [Reference scenarios](design/reference-scenarios.md): end-to-end acceptance
  scenarios for M3–M5, runnable without hosted repositories, CI, or pull
  requests

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
