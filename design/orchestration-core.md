# Orchestration core

**Status:** M1 contract for M3–M5. See [ROADMAP.md](../ROADMAP.md) and the
[Release inputs contract](release-inputs.md).

This contract defines environments, units, desired and observed state,
execution, recovery, deletion, the driver interface, the command unit, and the
effects of the orchestration commands. Promotion rules are in the
[promotion contract](promotion.md); this contract defines the state they
read.

Nothing here changes existing commands. A project without Environments and
units never touches orchestration state.

## Resources

| Resources | API group |
| --- | --- |
| Environment, EnvironmentGroup, EnvironmentTemplate, PromotionPath (and the existing GitRepository) | `gitops.nyl/v1` |
| Built-in unit kinds: `Command`, `Terraform`, `OpenTofu`, `OciImage`, `KubernetesPublication` | `units.gitops.nyl/v1` |
| Artifact kinds: `ContainerImage`, `PublishedTree` | `artifacts.gitops.nyl/v1` (see [Artifacts](#artifacts)) |
| Plugin unit and artifact kinds | The plugin's own groups, such as `units.acme.example/v1` (see [Plugin drivers](#plugin-drivers)) |

Every resource in a unit group is a unit; the group identifies the family and
the kind selects the driver, as `components.k8s.nyl/v1` does for component
invocations. Discovery follows Git visibility across the project, like the
Kubernetes GitOps resources. `nyl create` and `nyl delete` gain `environment`,
`environment-group`, `environment-template`, `unit`, and `promotion-path` resources; `nyl get` covers those declarations
and, with `-e`, an environment's units, outputs, artifacts, and promotions
(see [Inspection](#inspection)).

### Environment

```yaml
apiVersion: gitops.nyl/v1
kind: Environment
metadata:
  name: production
  labels: {stage: production}             # matched by -l and EnvironmentGroup selectors
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
    path: ''                                # optional directory within the refs; default the root
    coordinationRefPrefix: refs/nyl/        # where leases, runs, signals, and evidence live
    admissionRef: nyl/production/admission  # optional (M8); default the desired ref
  source: {revision: main, commit: 3e7b…}   # optional; omitted: the entry worktree's commit (see Source)
  protectedRefs: [main, 'release/*']        # default: each repository's default branch
  allowUnprotectedSource: false             # true lets runs use commits outside protectedRefs
  attestations: [qa]                        # environment-wide, recorded with nyl attest
  admission: required                       # optional (M8); CI deployment admission, default off
```

| Field | Type | Default |
| --- | --- | --- |
| `metadata.labels` | map of strings | none |
| `unitSelector` | label selector (`matchLabels`), as for `applicationGroupSelector` | `{}`: every unit |
| `values` | object | `{}` |
| `state.repositoryRef` / `state.repository` | GitRepository reference / inline GitRepository spec | the group's, else `origin`, else the local repository |
| `state.desiredRef`, `state.observedRef` | ref name | `nyl/<environment>/desired`, `nyl/<environment>/observed` |
| `state.path` | relative directory | `''` |
| `state.coordinationRefPrefix` | ref prefix ending in `/` | `refs/nyl/` |
| `state.admissionRef` (M8) | ref name | the desired ref |
| `source` | Git source without `path`, or `{fromPromotion}` | the entry worktree's commit |
| `protectedRefs` | list of ref names or globs | the group's, else each repository's default branch |
| `allowUnprotectedSource` | boolean | the group's, else `false` |
| `driftRepair` | `converge` or `manual` | the group's, else `converge` |
| `attestations` | list of attestation names | `[]` |
| `admission` (M8) | `off` or `required` | the group's, else `off` |

- An Environment is static at discovery, like a DeploymentTarget.
- `unitSelector` matches literal unit `metadata.labels` across all unit kinds;
  an empty selector selects every unit. The selected, enabled units are the
  environment's authoritative ownership set: leaving it is how deletion is
  requested (see [Deletion](#deletion)).
- Unit templates see only `values`, the sanitized Environment as
  `environment`, and, in template instances, `params`. They never see secrets,
  the process environment, or repository credentials; secrets reach a unit only
  through its `env` field at execution (see [Credentials](#credentials)).
- Environment names are unique across declared Environments and all
  EnvironmentTemplate instances, because leases, run refs, and keep refs are
  named after them.
- `state` follows the shape of DeploymentTarget `publication`: a GitRepository
  reference (`repositoryRef`) or inline `repository`, reading through its
  `repoURL` and writing through its `publishURL`. Without either, state uses
  the source checkout's `origin` remote, and without an `origin`, the local
  repository itself (see [Local runs](#local-runs)); CI should name a
  GitRepository.
- `desiredRef` and `observedRef` may name the same ref; the layout is identical
  either way (see [State layout](#state-layout)). A short name is a branch,
  the default, because state is the reviewable record: it can be protected,
  its history is visible in the forge, and a promotion pull request needs a
  branch as its base. A full `refs/…` name keeps state out of the branch list
  at the cost of that protection: forges cannot protect such refs, so anyone
  with push access can rewrite or delete the state. Validation warns about it
  with `NYL-STATE-UNPROTECTED`, and rejects it when a PromotionPath into the
  environment uses `changeGate: pullRequest`.
- `coordinationRefPrefix` places the refs Nyl creates and deletes on every
  run, `<prefix><env>/lease`, `<prefix><env>/runs/<run-id>`,
  `<prefix><env>/signals/<run-id>`, and `<prefix><env>/evidence/<run-id>`
  (see [Runs and leases](#runs-and-leases)). The default custom
  namespace keeps them out of the branch list and out of push-triggered CI;
  a branch prefix such as `refs/heads/nyl-run/` suits hosts that reject custom
  refs. Changing it goes through `nyl state move` (see
  [Initialization and moves](#initialization-and-moves)). Hosts verified with
  both prefixes are listed in [Git host compatibility](../nyl/book/src/content/docs/orchestration/environments-and-state.md#git-host-compatibility).
- Validation checks every configured state ref, and the coordination refs each
  environment implies, for names that Git cannot hold together, such as a
  shared state branch `nyl/previews` and an environment named `previews` whose
  desired ref would be `nyl/previews/desired`, and names both sides.
- `path` places the environment's state in a directory of those refs, so several
  environments can share one ref, each with its own `state.yaml`, `desired/`,
  and `observed/`. Leases stay per environment, so environments sharing a ref
  still run in parallel; their pushes serialize on the ref and, touching
  disjoint paths, rebase under the transition commit conflict rule. Branch
  protection and retention are then shared.
- `attestations` declares environment-wide attestations, such as `[qa]` for
  manual QA of the whole environment, which `nyl attest` without `--unit`
  records for the environment's current state (see [Attestations](#attestations)).
- `protectedRefs` lists the refs that pinned commits and run source commits
  must be reachable from; `allowUnprotectedSource` relaxes that for run source
  commits (see [Pinned commits](#pinned-commits)).
- `admission: required` makes state-writing runs work at the source of the
  current [deployment admission](ci-execution.md#deployment-admission), so a
  delayed or retried CI job cannot redeploy an overtaken source. The record
  lives in `admission.yaml` on `state.admissionRef`; giving that ref its own
  branch, pushable only by the dispatcher, separates admission authority from
  worker credentials. Both fields are read from the entry worktree, like
  `state`, so changing them is a reviewed change.

### Environment groups

Environments that belong together, such as the infrastructure and services
environments of one stage or every environment on one cluster, can be named as
an EnvironmentGroup. A group selects Environments by their literal
`metadata.labels`, the same labels a `-l` selector matches, and can carry
configuration they share:

```yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentGroup
metadata:
  name: production
spec:
  environmentSelector:
    matchLabels: {stage: production}
  values:                                   # defaults beneath each Environment's values
    cluster: prod-1
  state:
    repositoryRef: {name: platform-state}   # default state repository
  protectedRefs: [main]                     # default for the members
  allowUnprotectedSource: false
```

- Like an Environment, a group is static at discovery, and it selects declared
  Environments only. Template instances are reconciled as a fleet with
  `--template` (see [Template changes](#template-changes)).
- Shared fields are defaults. `values` merge recursively beneath the
  Environment's `values`, where the Environment wins, as target values win over
  Cluster values; `state.repositoryRef`, `state.repository`, `protectedRefs`,
  `allowUnprotectedSource`, `driftRepair`, and `admission` apply where the
  Environment leaves them unset (owner: `effective_environment` in
  `nyl_core::resources::environment`). A group sets no refs, paths, sources, or unit selectors, which stay each
  environment's own.
- The defaults belong to the Environment's configuration, not to the
  invocation: they apply whether an environment is reconciled through its
  group, with `-e`, or by a selector.
- Groups are read like the Environment itself (see [Source](#source)): the
  defaults for `state`, `protectedRefs`, `allowUnprotectedSource`,
  `driftRepair`, and `admission` come from the entry worktree, together with any GitRepository they name, because
  they are needed before the source commit S is known; `values` come from S.
  Execution keys depend only on what is read at S, so the same source commit
  always renders the same units and execution keys.
- Groups that only select may overlap. An Environment selected by more than one
  group that carries shared fields is a validation error naming the groups,
  because the order of their defaults would be arbitrary.
- An empty selector is rejected; a group lists its members explicitly by
  labels. `nyl get environments -g <group>` shows the members and where each
  effective shared field came from.

### Source

Every run of an environment works in one worktree at one immutable source commit S:
discovery, the Environment's own fields, targets, units, Releases, `fromFile`,
are all read there, and nothing else in the entry worktree is read again. The
one exception is a `carryFileFromWorktree` file for a `fromPublication` binding: it exists only
in the worktree of the job that produced it, so it is read from the entry
worktree and recorded as carried, as release-inputs.md describes. The Environment's `source` field decides S:

```yaml
spec:
  source:                               # omitted: the commit of the worktree you run from
    revision: release/prod              # follow a ref: each run uses its tip
    commit: 3e7b9c…                     # optional lock, refreshed by `nyl update source-locks`
    repositoryRef: {name: platform}     # optional; default: this repository
  # or, instead of the fields above:
  source:
    fromPromotion: {path: dev-to-prod}  # the commit recorded by the latest promotion;
                                        # `paths: [dev-to-prod, hotfix-to-prod]` for several
```

| `source` | S for each run |
| --- | --- |
| omitted | The entry worktree's commit, typical for dev and previews |
| `revision` | The tip of `revision`, such as a `release/prod` branch that carries hotfixes |
| `revision` and `commit` | `commit`, a reviewed pin in source that `nyl update source-locks` moves; never a promotion target |
| `fromPromotion` | The source commit in the newest PromotionRecord on that path, or on any of `paths`, kept in the target's desired state or, with `record: source`, in this field; before the first promotion, runs exit 2 (see [Promoting an environment's source](promotion.md#promoting-an-environments-source)) |

- The three forms exclude each other. `commit` requires `revision`, as for
  ApplicationGroup and unit sources: it must be reachable from `protectedRefs`
  or its own `revision`, and `nyl update source-locks` refreshes it with the
  other locks, `--check` included.
- Nyl reads five fields from the Environment in the entry worktree, because
  they decide S or where state lives: `source`, `state`, `protectedRefs`,
  `allowUnprotectedSource`, and `admission`, together with the
  EnvironmentGroup defaults for them and the GitRepository resources they
  name. Every other field is read at S; the copy of `source` inside S is ignored.
- State records S for every run (`desired/environment.yaml` and the
  `Nyl-Source-Commit` trailer). Runs that are not entered from the
  environment's own source use that record: the fleet reconcile, and a plain
  `reconcile -e <instance>`. The pull request job's
  `reconcile -e <instance> --template <name> --param …` moves an instance's
  recorded commit to its entry worktree, so a job run from another checkout
  never moves an instance.
- A recorded source commit that the entry clone lacks is looked up in the
  retained worktree snapshots of the state repository, then fetched through
  the instance's keep ref (`keepSource`), and then by its exact id from the
  clone's `origin`, with the credentials of the other Git fetches, and last,
  for a host that will not serve a commit by its id, through the instance's
  recorded `sourceRef` (below). Only the clone's `origin` remote is contacted and only
  the recorded id, or the recorded ref, is requested; a failed
  fetch leaves the commit missing, which the caller reports, and `--local`
  fetches nothing.
- `--source-ref <ref>` on `nyl reconcile -e <instance> --template <name>` and
  `nyl state init --template`, the commands that record an instance's commit,
  records a full ref name of the source repository's `origin` in the
  instance's `state.yaml` as `sourceRef`, next to `sourceCommit`. It is a
  hint, validated by its syntax alone (a full name under `refs/` following
  `git check-ref-format`; `NYL-INSTANCE-OPTION-INVALID` otherwise, before
  anything is written) with no network check. It is a flag of the job and not
  a field of the template, so a template is not specific to a pull request
  and works the same locally. A run that passes it overwrites the recorded
  ref; a run without it keeps the recorded one. The resolver is given the
  recorded ref as data from the instance record (never read globally), and
  after the local clone, the keep ref, and the fetch by id all miss, fetches
  that ref by name from `origin` with the same credentials, online only,
  writing no ref. It then checks whether the recorded commit is present; it
  never uses the ref's tip. If the commit is still absent the caller reports
  it missing as before (`NYL-SOURCE-UNAVAILABLE`), so a ref that moved on, or
  one the host dropped, can never substitute another commit for the recorded
  one. Rendering and teardown always use the recorded commit. A host that keeps a pull request's commits reachable after
  its branch is gone (GitHub's `refs/pull/<n>/head`, GitLab's
  `refs/merge-requests/<n>/head`) therefore serves a closed job running on a
  checkout of the default branch.
- An EnvironmentTemplate takes the same field and may template it from
  `params`, for example `revision: 'refs/pull/{{ params.pr }}/merge'`.
- `--source <rev>` overrides S for one single-environment `plan`,
  `reconcile`, or `teardown`, for example to preview what promoting a commit
  would do to prod, or to tear down after a recorded commit was lost. It never
  changes the environment's `source`: the run records it as that run's S, as
  every run does, and the next run uses the environment's source again. It
  must satisfy the same reachability rules, the transition commit marks it as
  an override, and a fleet reconcile rejects it. A full commit id that the
  entry clone lacks is fetched from the clone's `origin` like a recorded
  commit.
- `reconcile -e <env> --worktree` snapshots Git-visible tracked and untracked
  working files once, with deletions and executable modes. It leaves HEAD and
  the user's index untouched, preserves internal symbolic links without following
  them, rejects traversal through symlinked directories, and excludes ignored untracked
  files and the configured Nyl cache. Tracked files inside the cache cause capture
  to fail. Exactly one named environment is required; `--source`, label selectors,
  and group selectors conflict with this mode. The snapshot overrides the
  Environment's source selection and ordinary run-source reachability only;
  explicitly pinned unit source fields retain their own revisions and checks.
  Discovery, ownership preflight, rendering, execution keys, and every execution
  consume this snapshot, even if working files change during the run.
- Worktree snapshots are orphan synthetic commits recording HEAD as provenance, under local refs
  `refs/nyl-worktrees/<base>/<tree>`, with a `Nyl-Worktree-Snapshot: true`
  trailer. Identical snapshots reuse the retained commit. Persisted
  `sourceChosenBy` uses `by: override` and that reserved revision; the transition
  summary records `worktreeSource`, and `NYL-SOURCE-WORKTREE` explains the source
  and retention. They cannot establish clean promotion evidence or publish
  Kubernetes trees, including through `publish-tree` after checkout. An ordinary
  run cannot select a snapshot through `--source`.
- Before effects, the run commit retains the snapshot as an additional parent.
  State transition and retired evidence commits retain referenced snapshots
  likewise. State history follows first parents and stops before a source
  snapshot, including on fresh refs populated by a copy-based move. State and coordination
  fetches and state moves therefore preserve inputs for recovery and teardown
  from a fresh clone. The configured state repository archives Git-visible
  uncommitted files without source ancestry. `--local` uses that repository's
  local copy without contacting the remote; source selection is independent. These
  provenance forms use the existing v1 state documents.
- With `admission: required` (M8), the current
  [deployment admission](ci-execution.md#deployment-admission) chooses S for
  state-writing runs: admission resolves the environment's source by the rules
  above, and runs use the admitted commit, recorded with `admission` and the
  generation as how S was chosen. Before the first admission they exit 2 with
  `NYL-CI-ADMISSION-MISSING`. `reconcile --source` is refused, since a
  different source is a rollback admission; `plan --source` still previews.
- A run keeps its S to the end. When a promotion or a source change lands
  while it runs, its final commit is resolved again against the new state but
  at the same S, and the next run picks up the new source.
- A unit's own source fields have the same shape plus `path` (see
  [Units](#units)) and say where the unit builds from, such as an image's
  build contexts or a Terraform module at another revision or in another
  repository. They default to S. The environment's source says
  where units are defined; a unit's source says what they build.

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
- Units follow the rules of the other templatable control resources. A file
  is split into YAML documents before anything renders, each document is one
  resource, and a template never spans documents or generates several units:
  composition happens through selection and values. A document that is not
  valid YAML before rendering must write its envelope in block style, one
  literal key per line, because discovery then reads it with the static
  envelope scanner, as it does for ApplicationGroups today. Below the envelope, a
  document may use structural templating, including text that is not valid
  YAML before rendering, so an environment's resources can be colocated in one
  file.
- A unit document that renders to nothing counts as `enabled: false`, the
  same as an ApplicationGroup that renders to nothing. `plan` reports it as
  leaving because it rendered empty, since discovery knows it from its
  envelope.
- `metadata.name` stays literal. It is the unit's identity for references,
  receipts, leases, and deletion, and a templated name would turn a value
  change into a deletion and a new incarnation. Unit names are already scoped
  per environment, and names that must differ on the Kubernetes side have
  their own templated fields.
- Every unit kind shares the common fields below. They are one Rust struct
  flattened into each kind's spec, so their schema and documentation are
  identical everywhere. The remaining fields belong to the kind, and each kind
  has its own schema and generated reference page.

  | Field | Type | Default | Meaning |
  | --- | --- | --- | --- |
  | `enabled` | boolean | `true` | Evaluated after rendering; `false` leaves the ownership set |
  | `outputs` | map of name to output declaration | `{}` | Declared outputs, for kinds that let units declare them (see below) |
  | `observations` | map of name to `{type, maxAge?, from}` | `{}` | Observed values, for kinds with the `observed_values` capability (see [Observed values](#observed-values)) |
  | `observationWait` | duration | `0s` | How long a run that executed the unit waits for its `pending` observations, within `timeout` |
  | `dependsOn` | list of unit names or `{environment?, unit, evidence?, attestations?}` | `[]` | Ordering without a data reference |
  | `approval` | `auto`, `manual`, or `{mode, bind, requireDigest}` | `auto` | See [Approval](#approval) |
  | `deletionPolicy` | `Teardown` \| `Retain` | `Teardown` where the kind supports teardown, else `Retain` | See [Deletion](#deletion) |
  | `timeout` | duration | per kind (see [Defaults](#defaults)) | Deadline for one execution, teardown, drift detection, or attestation; at most `7d`, since a run holds its lease for it |
  | `env` | `{passthrough: [name], secrets: {name: key}}` | `{}` | See [Credentials](#credentials) |
  | `attestations` | list of `{name, from: external}` | `[]` | Attestations only external sources supply (see [Attestations](#attestations)); kinds that attest themselves add their own names |

- References may appear in any field the kind's schema marks as accepting
  them, at any depth inside object and array values; a reference object
  replaces the whole value it stands for, never part of a string. Every Git
  source field (`source`, `context`, directory `contexts`) accepts
  `fromPromotion` for a promoted source. Besides those, `Command` accepts
  references in `values`; `Terraform` and `OpenTofu` in `variables`;
  `OciImage` in `buildArgs` and image `contexts`; and `KubernetesPublication`
  in the inline target's `releaseInputs`.
- A kind either publishes fixed results as artifacts (`OciImage`,
  `KubernetesPublication`; see [Artifacts](#artifacts)), in which case
  `outputs` is rejected, or lets the unit declare outputs (`Command`,
  `Terraform`, `OpenTofu`). Declared outputs use
  the Release input type set (`string`, `integer`, `number`, `boolean`,
  `object`, `array`), optional `description`, and optional `sensitive`. Only
  declared outputs are recorded. A `sensitive`
  output is validated but never persisted and cannot be referenced; secrets
  move through the secrets provider, not through outputs. Undeclared outputs
  are ignored by every kind, sensitive or not; the Command kind warns about
  them. A declared output that the native tool marks sensitive must be
  declared `sensitive`, and drivers check this before any effect where the
  tool allows.
- `dependsOn` lists units, by name or as `{unit, evidence}`, that must have a
  current receipt first, for ordering without a data reference. References add
  dependencies implicitly. An entry with `environment` names a unit of another
  declared environment, under the rules of
  [Cross-environment references](#cross-environment-references).
- A `dependsOn` entry or a `fromUnit` reference may require stronger evidence
  from its producer with `evidence: published | attested` (default
  `published`, which is a current receipt) and `attestations`, which names the
  producer's attestations to require instead of all it declares (see
  [Attestations](#attestations)). Naming an attestation the producer does not
  declare is a resolution error (`NYL-ATTESTATION-UNDECLARED`, also reported by
  `nyl validate`). So is a requirement that would include a name declared
  only with `from: external`, by name or through `evidence: attested`
  (`NYL-ATTESTATION-EXTERNAL-UNSUPPORTED`, also reported by `nyl validate`):
  nothing can record an external attestation until `nyl attest --name …
  --result …` ships (M6), so the consumer would wait forever. Its fix says so
  and names the attestations the producer's driver observes. For a producer
  in another environment, `nyl validate` cannot see the names its driver
  reports, so `plan` and `reconcile` report these errors. A consumer whose producer lacks the evidence is `blocked` on
  it, exit 2, until the producer's current receipt has every required
  attestation passing (owner: `rules::evidence`):

  ```yaml
  dependsOn:
    - {unit: kubernetes, attestations: [healthy]}   # smoke tests run against healthy workloads
  ```

  A plan, status, or reconcile report shows the consumer `blocked` at the
  entry or reference with `blockedBy` reason `attestation-pending` (none is
  recorded for the producer's current receipt) or `attestation-failing` (the
  recorded one is `fail`), naming the producer (with its environment, when it is another
  environment's unit) and the attestation, and the
  warnings `NYL-ATTESTATION-PENDING` or `NYL-ATTESTATION-FAILING`. Each
  warning carries its fix: `nyl attest -e <env>` and a new reconcile, or
  fixing what failed. A producer without a current receipt is blocked first
  (`no-current-receipt`). Every `reconcile` resolves again, so a consumer
  runs in the first run after `nyl attest` records the pass, and a unit that
  attests itself during `reconcile` unblocks its consumers in a later wave of
  the same run.
- Every kind that reads files names them in fields with the one Git source
  shape used everywhere, `source` for most kinds and `context`/`contexts` for
  `OciImage`: `path`, an optional `revision` and
  `commit` lock, and an optional `repositoryRef` or `repository`. Omitted
  fields fall back step by step: without a repository, this repository;
  without `revision`, the environment's source commit S. A field that names
  another repository must name its `revision`, because S is a commit of this
  repository. Most units set only `path`.
  - In this repository, at any revision, `path` follows the one local path
    rule that local ApplicationGroup sources use: relative to the directory
    containing `nyl.toml`, with leading `..` segments allowed, or from the
    worktree root with a leading `/`, never leaving the worktree or traversing
    a symbolic link. The same rule covers `files` globs, Command
    `workingDir`, and `fromFile`. In another repository, `path` is relative to
    its root. Resolution records every such path in `resolvedSpec` in its
    worktree-rooted form, such as `/infra/database`, so the desired document
    means the same wherever `nyl.toml` lives.
  - `revision` alone resolves the ref's tip during resolution; the desired
    document records the resolved commit in `resolvedSpec.source.commit`, so
    every run is reproducible from state.
  - `revision` with `commit` is a pin, with the reachability rule of
    [Pinned commits](#pinned-commits); `nyl update source-locks` refreshes it
    with the other locks.
  - A PromotionPath selector on `/source` promotes a unit's build source,
    which matters when a unit builds from another repository that the
    environment's source does not pin.

### Identity

- Unit names are unique per environment across all unit kinds, so references
  name only the unit. A unit's address is `<environment>/<unit>`.
- Each incarnation has a `uid`, a UUIDv7 generated when resolution first
  creates its desired document in a run. Runs of one environment are
  serialized by its lease, so only one run can create it. The run's
  checkpoints carry every desired document it created, with its uid, so a run
  that takes over after a crash adopts them (see
  [Runs and leases](#runs-and-leases)) and a new unit's checkpointed result is
  never mistaken for another incarnation's.
- The uid fences every receipt, checkpoint, and deletion. A name whose desired
  file is `deleting` cannot start a new incarnation until the deletion
  completes.
- Changing a unit's `apiVersion` or `kind` under the same name ends the old
  incarnation: it is deleted under its deletion policy, and the new kind
  starts a new incarnation. A unit never inherits another kind's receipts.
  A teardown caused by a kind change requires `--allow-teardown`, like one
  caused by omission, so a kind change never destroys resources by accident.
- An explicit teardown closes an incarnation even while the unit stays in the
  ownership set; the next incarnation receives a new uid (see
  [Deletion](#deletion)).
- **Native ownership.** Different unit names do not imply disjoint native
  resources. Each driver reports the ownership scopes a resolved spec claims
  (`ownership_scopes`, see [Drivers](#drivers)): a Terraform or OpenTofu
  backend and key with its workspace, an `OciImage` registry repository with
  the `nyl-<environment>-` tag prefix it moves, and a `KubernetesPublication`
  target's publication repository, revision, and prefix. Resolution rejects
  two units, in the same or in different declared environments and template
  instances, that claim the same scope, and names both
  (`NYL-UNIT-OWNERSHIP-OVERLAP`). Scopes a driver cannot establish, such as
  what a command touches, are not checked. Environments may share a registry
  repository, because each has its own tag prefix.

### References

| Reference | Resolves to | Freshness |
| --- | --- | --- |
| `fromUnit: {unit, output, pointer, evidence, attestations}` | A declared, non-sensitive output of a unit in the same environment; `pointer` optionally selects inside an `object` or `array` output | The producer's receipt must be current, with the required `evidence` |
| `fromUnit: {unit, artifact, kind, pointer, evidence, attestations}` | A field of an artifact the unit published; `kind` optionally asserts the artifact kind | The producer's receipt must be current, list the artifact's digest, and have the required `evidence` |
| `fromUnit: {unit, observation, pointer, evidence, attestations}` | The latest recorded value of a unit's [observed value](#observed-values) for its current receipt | The producer's receipt must be current, with the required `evidence`, and the observation must have a value (not `pending`, not older than its `maxAge`) |
| `fromPromotion: {value, path}` | A value in a PromotionRecord in this environment's desired state: without `path`, the newest record carrying the value, limited to the source paths when the environment's source is promoted (see [Several paths into one environment](promotion.md#several-paths-into-one-environment)) | The record must exist; broken selectors are errors |
| `fromUnit: {environment, unit, output \| artifact \| observation, …}` | An output, artifact, or observed value of a unit in another, declared environment (see [Cross-environment references](#cross-environment-references)) | As `fromUnit`, against the other environment's current receipt |

- `pointer` is a JSON Pointer ([RFC 6901](https://www.rfc-editor.org/rfc/rfc6901)):
  `/` separates tokens, `~1` escapes `/`, and `~0` escapes `~`. An empty or
  omitted pointer selects the whole output; an artifact pointer is relative to
  the artifact's `spec`. PromotionPath selectors and `nyl get … --pointer` use
  the same resolver.
- The same reference forms are Release input bindings on a target that a
  publication unit places into an environment, `environment` included.

#### Cross-environment references

Any environment may read units of a declared environment, for example when
services in one environment consume the cluster facts that an infrastructure
environment on the same cluster produces, or when previews use dev's network.

- The producer must be a declared environment; template instances are
  consumers only, because they are too short-lived to depend on. A reference
  or `dependsOn` entry whose `environment` names an instance, from a declared
  environment or from another instance, is refused with
  `NYL-REFERENCE-INSTANCE-PRODUCER`, naming the consumer and the instance,
  in source validation and in every run, before anything executes
  (`nyl_orchestration::rules::multi_environment::instance_producers`, and the
  run shell's producer snapshots). A declared environment and an instance of
  the same name are the declared environment. An instance reads a declared
  environment exactly as a declared environment does, including the wait, the
  single snapshot per run, and the provenance.
- The graph of environments that reference each other must be acyclic;
  validation rejects a cycle and names it.
- A reference is read-only and blocks like any reference, exit 2, while the
  producer's receipt is not current. Separate `nyl reconcile` calls therefore
  converge in any order: a consumer run before its producer waits, and the
  next run after the producer resolves.
- A run reads each producer environment's desired and observed tips once, when
  resolution first needs them, and resolves every reference to that
  environment against that one snapshot for the rest of the run; provenance
  cites the producer's observed commit. The consumer never takes the
  producer's lease.
- A unit that other environments reference is protected like a referenced unit
  in its own environment. `plan` lists the environments and instances that
  reference it, and a teardown, replacement, or omission of it, including
  `state delete --teardown` of its environment, refuses while they reference
  it unless the invocation passes `--allow-dependents`, which the transition
  commit records. Nyl finds dependents by reading the declared environments'
  desired state and listing the templates' state locations, so a live
  instance's unit protects its producer like a declared consumer.
- Omitting a producer from its environment is refused like its teardown: the
  unit leaves the ownership set, its teardown is pending, and it waits
  (`NYL-TEARDOWN-HAS-DEPENDENTS`) while a live instance's stored unit
  references it, until `--allow-dependents`. The consumer, not the producer,
  then carries the error: when the sources of the environments are validated
  together (instances at their recorded commits), a consumer that still
  references a unit its producer no longer selects is
  `NYL-REFERENCE-INVALID` ("outside its ownership set"), reported on the
  consumer's environment alone, so its own next run fails or blocks while the
  producer's environment, and every unrelated one, runs. The same holds for a
  reference that names an instance (`NYL-REFERENCE-INSTANCE-PRODUCER`): one
  preview's bad reference never stops dev. An expired instance being removed
  is not rendered, so it never holds a producer back.

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
  target: '{{ values.target }}'   # a static DeploymentTarget, or `inline:` (see below)
  observe: {}          # opt in to observing the target's reconciler; omit to publish only
  teardownWait: {strategy: manual}   # observe | delay | manual; see Teardown
```

- `target` names a static DeploymentTarget, or defines one inline as a
  DeploymentTarget spec rendered per environment and owned by the unit:

  ```yaml
  spec:
    target:
      inline:
        clusterRef: {name: preview-cluster}
        applicationGroupSelector: {matchLabels: {preview: 'true'}}
        publication:
          repositoryRef: {name: deploy}
          revision: previews
          pathPrefix: '{{ environment.name }}'
        values: {hostname: '{{ values.hostname }}'}
        releaseInputs:
          platform/web:
            image: {fromUnit: {unit: web-image, artifact: image, pointer: /reference}}
  ```

  An inline target is named `<environment>-<unit>`, so adding or removing
  publication units never renames another unit's target. `target.name`
  overrides the name; it must be unique like every target name, declared or
  inline. A unit publishes to one destination: its target's name and
  publication repository, branch, and prefix, which each `PublishedTree`
  records. The engine has no generic replacement on an identity change (only
  a kind change replaces an incarnation), so a changed destination does not
  move the published tree: the driver fails before any effect with
  `NYL-KUBERNETES-TARGET-CHANGED`, retryable (restoring the destination
  heals it), whose fix is to replace the unit with
  `nyl teardown -e <env> --unit <u>`, which tears down the stored document's
  old target, and then reconcile. A later milestone may let a driver declare
  its identity fields so the engine replaces the unit itself.

  The unit renderer fills the target's `name` and `sourceFile`, the
  project-relative file that declares the unit (the file `valuesFrom` paths
  of the inline spec are relative to); both are renderer-owned, so an authored
  `sourceFile` is an error. From there the inline target is an ordinary
  target: it joins the project's inventory
  (`GitOpsInventory::with_inline_target`), is parsed as strictly as a
  discovered one, and renders, validates, and publishes through the same
  compile and publish path. The `fromUnit` bindings of its `releaseInputs`
  expand into `releases` as for any target. Reference resolution replaces each
  such binding in the stored spec with the value it resolved, so the driver
  renders the inline target without the bindings the pinned `releases`
  supply, which the render takes from the snapshot.
- **Why both exist.** A DeploymentTarget says what is rendered and where it is
  published; it is complete without orchestration and is what a project
  without environments writes. A KubernetesPublication is the
  environment-independent role an environment executes: one definition, such
  as `kubernetes`, is instantiated per environment and places one target into
  each. Promotion selectors and coverage name that shared unit in every
  environment, templated preview environments need its inline target, and
  deselecting it starts a teardown, which a plain target never implies.
- Generated Argo CD Application and AppProject names must be unique among all
  targets whose Argo CD instance resolves to the same cluster and namespace
  (a direct target generates none, see
  [Direct reconciler](#direct-reconciler)):
  static targets on explicit or implicit instances, and inline targets alike.
  Validation checks this across every target it can see, including every
  instance of an EnvironmentTemplate, whose inline targets must therefore
  template their names, for example from `environment.name`. M2 extended the
  check from targets sharing an explicit ArgoCDInstance to this rule; inline
  targets and template instances join it with M5. As implemented:
  the inline targets of every environment's enabled publication units join
  the inventory of every compile that checks across targets, in `nyl validate`
  and in every publication render (`gitops::units::with_inline_targets`,
  where the rendering unit's own target comes from its stored document), so
  prefix overlaps and Argo CD name collisions are found whichever entry point
  compiles; a disabled unit is torn down from its stored document and
  contributes none. Template instances join with
  EnvironmentTemplates.
- The unit places its DeploymentTarget into the environment. A target
  referenced by publication units in two environments is an error; a target
  referenced by none rejects `fromUnit` bindings and reads `fromPromotion`
  from its own `spec.promotions` (see
  [Promoting into a target](promotion.md#promoting-into-a-target)). A
  publication unit that is `deleting` still owns its target until its
  teardown completes or it is forgotten, so deselecting it does not make the
  target's bindings invalid.
- The ownership index of every tree the unit publishes records the owning
  environment and unit uid next to the existing target, cluster, and
  publication identity. The `owner` is an optional member of the version 3
  index, which also carries per-Release ownership and mixed provenance (see
  [partial publication](partial-publication.md)); orchestrated publications
  write it and non-orchestrated publications do not. Publish and teardown refuse a prefix whose index names
  another environment's or another incarnation's unit while that incarnation
  still exists in its environment's state, active, held, deleting, or
  retained. A
  target moves between environments once the old environment's incarnation is
  gone: torn down, or released with `nyl state forget` (see
  [Deletion](#deletion)). The first publish of the new owner then records
  itself in the index.
  - The index's `owner` is `{environment, unit, uid}`, or the string
    `catalog:<name>` for a shared catalog's tree (see Catalog ownership); only
    version 3 can name one (owner: `nyl_render::gitops::reconcile`, with a
    golden file).
    Version 2, which is released, and a version 3 index without an `owner`,
    as `publish-tree` writes it, name no owner and stay readable. The
    first orchestrated publication over one adopts it, if the target's
    identity matches, and records its owner even when no manifest byte
    changes. A publication that records no owner, such as `publish-tree` and
    `render-tree`, refuses a prefix whose index names an owner, because it
    cannot tell whether that unit still exists.
  - Whether an incarnation still exists is the shell's fact, not the
    driver's: `ExecutionContext::incarnations` answers it from each declared
    environment's state (`IncarnationDirectory`, implemented in
    `nyl_orchestration::shell::run::incarnations`). The incarnation exists
    when that environment's desired documents hold the unit name with that
    uid, in any lifecycle. The driver owns the rule: it takes a prefix over
    only when the owner is `gone`, and fails closed when state cannot say
    (`unknown`: the environment is not declared at the source commit, or its
    state cannot be read). Calls outside a run, such as drift detection and
    `nyl build`, pass a directory that answers `unknown` for everything; a
    direct `--push` records no owner and so never takes a prefix over (see
    Direct builds below).
  - A refused publication or removal fails with category `ownership`,
    retryable, because the next run reads state again; nothing was written.
    The code says why: `NYL-KUBERNETES-OWNER-FENCED` while the owner exists,
    and `NYL-KUBERNETES-OWNER-UNKNOWN` when state cannot say (an environment
    that was renamed or removed, or unreadable state). Publication outside
    orchestration refuses with `NYL-TREE-OWNED-BY-UNIT`. The way out of the
    last two, when the owner can no longer release the prefix, is deliberate:
    commit to the publication branch a change to `<prefix>/_nyl/index.json`
    that removes `owner`, which makes the prefix
    unowned, so the next publication adopts it. Drift detection over a fenced
    prefix is `unknown` with the same code.
  - Every answer of the directory reads state again, so an incarnation
    created since an earlier answer is never taken over.
- Its dependencies are the units named by the target's `fromUnit` bindings and
  the PromotionRecords named by its `fromPromotion` bindings.
- Its execution key covers the target render's complete input record from
  Nyl's dependency recorder, plus the resolved spec (`observe`, the Release inputs under `releases`) and the recorded `reconciler` facts of a unit that observes (everything the render derives from them, never how the reconciler is read): every file and directory listing,
  value, chart, Kubernetes capability, resolved source identity, and external
  tool fingerprint that can affect rendered bytes. Resolution renders the
  target to obtain it, through the render cache, so an unchanged target is
  cheap. Unlike other kinds, a publication's key therefore changes when a
  renderer or tool upgrade changes what rendering depends on. Publishing an
  unchanged tree creates no publication commit; only the receipt records the
  new key.
  - The key uses a portable form of the record: repository-relative paths and
    content digests, never machine paths or cache-local keys.
  - Secret inputs, admitted only when the invocation allows them (as for tree
    commands today), enter the key through a keyed fingerprint whose key comes
    from the project's secrets provider, so rotating a rendered secret
    republishes and no guessable digest of it is stored.
  - A render the recorder marks uncacheable, because an input cannot be
    observed completely, has no stable key: the unit executes on every
    reconcile.
- Its resolved spec contains every Release input of its target under
  `releases`, keyed by `<group>/<release>` and then by input, so the pointer to
  an input is `/releases/<group>~1<release>/<input>`: `value`, `fromFile` at the source
  commit, locked `fromGit`, `fromUnit`, `fromPromotion`, and `fromPublication`.
  - As implemented: `releases` holds the bindings only
    orchestration can resolve: `fromUnit`, resolved through the shared
    reference resolver in both its `output` and `artifact` forms, and
    `fromPromotion` once PromotionPath selectors exist (M6). The driver passes
    `releases` to the render as the pinned input snapshot, which takes an
    override's precedence over the binding and is checked against the
    Release's declared input types, so an unusable value fails before
    anything is published. `value`, `fromFile`, locked `fromGit`, and
    `fromPublication` inputs resolve inside the render at the source commit
    and enter the execution key through its input record, so a change to any
    of them still re-executes the unit. Every resolved input, whatever its
    source, is recorded in the published ownership index as
    `@input/<group>/<release>/<input>` with the digest of its canonical
    value. The renderer owns `releases`: an authored `spec.releases` is
    rejected.
- `fromPublication` is resolved like every other binding. Resolution reads the
  state file from the publication branch head B, or the carried file from the
  runner's working tree. The value enters the resolved spec and the execution
  key; B is recorded only as provenance. A new state file therefore changes
  the execution key and triggers an ordinary execution, while unrelated commits
  on a shared publication branch, including Nyl's own publications, change
  nothing.
- At execution, `publish-tree` builds on the current branch head. If the head
  has moved past B, including when another target's publication wins the
  compare-and-swap, and neither the state file nor any file the target owns
  changed, it rebuilds its commit on the new head and retries, a bounded
  number of times; such commits touch disjoint files. If the state file
  changed, the execution ends without publishing and the unit is resolved
  again in the next wave. If a file the target owns changed outside Nyl, the
  execution fails as a non-retryable ownership violation, as reconciliation
  does today.
- It publishes a `PublishedTree` artifact named `tree` with the published
  commit and ownership-index digest (`published`). A unit that declares
  `observe: {}` additionally lets `nyl attest` read the reconciler that runs
  its target (Argo CD) and attest `accepted` and `healthy`, per the promotion
  contract's [health evidence](promotion.md#health-evidence) and its
  [Reading Argo CD](promotion.md#reading-argo-cd) (owners:
  `nyl_units::kubernetes_publication::reconciler` for the shared rules and
  `nyl_units::kubernetes_publication::argocd` for the Argo CD readers). The
  publication still happens first: `observe` adds a reading, makes the
  `observe` strategy available for `teardownWait`, and makes it the default.
  `observe` is an object so that options can be added later; `{}` and
  `waitHealthy` (see [Waiting for health](#waiting-for-health)) are accepted. It does not say which reconciler is read: that follows the
  reconciler of the unit's target (its ArgoCDInstance, or its direct
  reconciler), so the same declaration observes whatever runs the target. Without `observe`
  the unit only publishes, its default `teardownWait` is `manual`, and
  `teardownWait: {strategy: observe}` is a validation error. A target may instead declare
  `reconciler: {direct: {}}`: Nyl then applies its Releases itself, after
  publishing, and `observe` reads Nyl's own release state in the cluster (see
  [Direct reconciler](#direct-reconciler)).
- It supports [observed values](#observed-values) with `observe: {}`, whose
  credentials it needs (a unit that does not declare `observe` and declares
  `observations` is rejected with `NYL-OBSERVATIONS-UNSUPPORTED`). Its `from`
  names a `resource` (`apiVersion`, `kind`, `namespace` for namespaced kinds,
  `name`) and a JSON `pointer` into it:

  ```yaml
  spec:
    observe: {}
    observations:
      ingressIp:
        type: string
        from:
          resource: {apiVersion: v1, kind: Service, namespace: ingress, name: ingress-nginx}
          pointer: /status/loadBalancer/ingress/0/ip
    observationWait: 10m
  ```

  - The resource must be one the target renders. The driver checks this when
    it renders the target to resolve the unit, and rejects one outside the
    rendered tree with its own code, `NYL-KUBERNETES-OBSERVATION-UNOWNED`, so
    the cluster is the target's Cluster and nothing outside the declaration
    is read. It rejects Secrets and the whole-resource pointer (`pointer: ""`), but a
    ConfigMap or a custom resource can still hold sensitive data, which the
    declaration author must not observe. A manifest without `metadata.namespace`
    belongs to the namespace its Application deploys to. Resolution records,
    in the resolved spec, the deployment (Application) that owns each observed
    resource, by its id in the reconciler facts
    (`observationSources`, part of the execution key) and the target's
    workload Cluster with its local context (`workload`; the context, which
    only says how to read, is left out of the key), so `observe` renders
    nothing.
  - The driver reads the resource from the target's workload Cluster, only
    once the Application that owns it runs the current receipt's
    publication, matched as for `accepted` (see
    [Kubernetes publications](promotion.md#kubernetes-publications)). Only
    that Application must match, so an observation whose Application did not
    change in a publication resolves at once, and a slow, unrelated
    Application does not hold it back. Until then it is `pending`
    (`application-not-running-publication`); afterwards a pointer that does
    not resolve yet is `pending` (`field-not-set`), because controllers fill
    in status after Argo CD applies the manifest. A missing resource, or a
    value of the wrong type, after the Application runs the publication is
    `failed`.
  - **Credentials.** The workload Cluster may differ from the Argo CD
    control-plane Cluster, so its reads name their own context: the
    workload Cluster's `spec.live.context`, in the kubeconfig the unit
    admits as `KUBECONFIG` (the one that reaches Argo CD under the
    `kubernetes` access method, when that kubeconfig holds both contexts, or
    one file of a list). Argo CD is read as `nyl attest` reads it, through
    the way its ArgoCDInstance declares. As for every driver credential, a
    variable the unit does not admit is a failed read that names the
    variable and the `env` entry that admits it; nothing falls back to the
    process environment, the current kube context, `~/.kube/config`, or
    in-cluster configuration, and a Cluster without a declared context
    cannot be read. A job that holds the read credentials a reconcile may
    lack runs [`nyl observe`](#observed-values).

#### Waiting for health

`observe: {waitHealthy: {timeout: <duration>}}` is opt-in; absent, reconcile does
not wait. After a successful publish (and, for a direct target, the apply), the
driver polls the unit's `ReconcilerReader` and runs the shared `evaluate` until
`healthy` passes, up to `timeout` and never past the unit's own deadline, then
reports the `accepted` and `healthy` attestations `nyl attest` would record,
which the engine records with the receipt (`source: {driver: reconcile}`). It
works for a direct target and for both Argo CD access methods, with no
reader-specific code (owner: `nyl_units::kubernetes_publication::health_wait`).

- **Time.** The wait uses the injected clock the execution context carries
  (`ExecutionContext::clock`): it reads `now` and waits through it, so a test
  clock advances instead of sleeping and nothing reads the system clock. Polls
  back off, a pause of 2 seconds doubling to 30.
- **Never a pass.** An unreadable reconciler, a reading whose absences are not
  provable (no anchor, a credential that cannot see), and a publication that
  does not match are `Unread` or `NotYet`, never `healthy`. A credential or
  declaration the wait cannot fix (`credentials`, `declaration`, `forbidden`,
  `rejected`) ends it at once as a retryable `credentials` failure; anything
  else retries until the timeout. A target the reader cannot see yet, an anchor
  or deployment reported hidden (an Argo CD catalog Application the operator has
  not applied yet, which is the normal state of a first deploy, since the
  catalog exists on the branch only after this publish), is `NotYet`, not an
  error: the wait polls until the timeout, whose failure names the reason and
  that the catalog must be applied. The reader says which it is: `NotYet` (the
  catalog Application or AppProject is not in the reading) keeps polling, while
  `Hidden` (an explicit RBAC denial, such as Argo CD's `can-i` answering no for
  an Application) fails at once like any denied credential. Only the wait treats
  `NotYet` so; `nyl attest` and `observe` still report such a reading as unread.
- **Early failure.** A `Degraded` reading ends the wait before the timeout only
  when the reader marks it terminal (`Observed::terminal`): the direct reader
  does for a rollout past its progress deadline and a failed Job, and Argo CD's
  reader never does, because its controllers retry. A terminal reading fails the
  unit with `NYL-KUBERNETES-HEALTH-DEGRADED`.
- **Timeout.** The unit fails, retryable, with category `timeout` and
  `NYL-KUBERNETES-HEALTH-TIMEOUT`, naming the worst deployment and its last
  health. It fails rather than succeeds with a warning because the author asked
  for the wait: a receipt would tell dependents that the target is healthy, or
  at least settled, which it is not. The tree is published and applied, so the
  next execution (recovery `converge`) publishes nothing new, applies again, and
  waits again; no receipt exists in between.

**Direct builds.** The unit declares the `build` capability
(`nyl build <unit> [-e <env>] [-- --diff | --push]`, see
[Direct builds](#direct-builds)), so the tree can be rendered, compared with
the published one, and, for a target no environment owns, published, without
orchestration:

- A build renders the unit's target through the path a run takes: the same
  renderer, the same pinned `releases` snapshot (resolved against the
  environment's receipts with `-e`, exactly as `nyl get output` does), no
  process environment, and no project secrets. It owns nothing, takes no
  lease, and writes no receipt or state. The files are those of the
  worktree, uncommitted changes included, or of `--source <rev>`; the
  orchestration shell hands the driver a snapshot of them committed as a
  Git worktree (one fixed commit), which the render needs, and the ownership
  index names the unit's source commit, never the snapshot's.
- Without `-e`, a target whose Release inputs bind `fromUnit` fails like any
  reference (`-e` names the way out). A unit that declares its target inline
  cannot be built without `-e`: the inline target is named
  `<environment>-<unit>` and rendered with the environment's values, so the
  build refuses with `NYL-KUBERNETES-BUILD-INLINE-TARGET`.
- With no argument, the build prints a `RenderedTree` document: the target's
  publication `repository`, `branch`, and `pathPrefix`, the `target`, the
  digest of every file beneath the prefix (`files`, by path in the
  repository), and the `indexDigest` a publication of exactly this tree would
  record.
- `--diff` compares the render with the tree published at the destination,
  reading it and writing nothing, and prints a `TreeDiff` document: `base`
  (the branch head, or `null` before it exists), `changes` (`none`, `create`
  when nothing is published at the prefix, or `update`), the `owner` the
  published index names, and the `files` that differ with their `change`
  (`added`, `modified`, or `removed`). The ownership index is compared by the
  files it records, and files in the prefix that it does not own are not part
  of the tree. The comparison is of files only: `changes: none` can still be
  followed by a `--push` commit that only updates `_nyl/index.json`, such as
  for a new `sourceCommit`. A difference is not a failure: like `diff-tree`, the exit is
  0 whether or not the trees differ, and a pipeline reads `changes`. An owned
  prefix can be compared, because only publishing is fenced.
- `--push` publishes the render as one commit, by compare-and-swap with the
  retries of a run's publication, and prints the `PublishedTree` document a
  run records, without recording it. It publishes only a target no
  environment owns, and refuses otherwise, before anything is published,
  with a stable code and the fix. `NYL-KUBERNETES-BUILD-TARGET-OWNED`,
  `NYL-KUBERNETES-BUILD-DIRTY-SOURCE`, and `NYL-KUBERNETES-BUILD-IDENTITY`,
  checked in that order, come before the build's own render and clone (the
  unit was prepared, and so rendered for its key, before the driver ran);
  `NYL-TREE-OWNED-BY-UNIT` comes when the publication prepares, after them:
  - `NYL-KUBERNETES-BUILD-TARGET-OWNED` when a publication unit that an
    Environment selects places the target (`target_placements`: even a unit
    being torn down, because it owns its target until its teardown
    completes). The fix names `nyl reconcile -e <environment>`.
  - `NYL-TREE-OWNED-BY-UNIT` when the published ownership index names a unit
    incarnation. The publication records no owner, so it cannot tell whether
    that unit still exists; the way out is the one above, a deliberate
    release of the prefix.
  - `NYL-KUBERNETES-BUILD-DIRTY-SOURCE` when the worktree has uncommitted
    changes, because the index attributes the tree to a commit that must hold
    it; the fix is to commit, or to build a revision with `--source`.
  - `NYL-KUBERNETES-BUILD-IDENTITY` when no identity is known to author the
    commit (`user.name` and `user.email` of the repository's Git
    configuration, or the wiring's author).

  The index of a direct publication has version 3 and no `owner`, as
  `publish-tree` writes it, so an environment that later selects the unit
  adopts the prefix.
- `--shared-catalog` selects the shared catalog Application of a target with
  `catalogApplication.shared` instead of the target's own tree, and composes
  with `--diff` and `--push`: the plain form prints a `RenderedTree` of the
  catalog (its prefix `_nyl/shared/<name>`, and `target` `catalog:<name>`),
  `--diff` a `TreeDiff` whose `owner` is the string `catalog:<name>` when the
  branch holds the tree, and `--push` publishes it, creating or replacing the
  manifest as the one writer of it. No environment owns a shared catalog, so
  `--push` is not refused as `NYL-KUBERNETES-BUILD-TARGET-OWNED`; the dirty
  source and identity refusals apply, and a prefix owned by anything but
  `catalog:<name>` is fenced. A target that shares no catalog is refused with
  `NYL-KUBERNETES-BUILD-NO-SHARED-CATALOG`.
- Any other argument, or `--diff` with `--push`, is refused with
  `NYL-KUBERNETES-BUILD-ARGUMENT`. The documents are local build information:
  nothing records them, and no reference reads them.

**Teardown.** A publication unit supports teardown when its target is
configured so that removing the manifests removes the workloads. It proceeds
in two phases, both through Git (a direct target's two phases delete in the
cluster instead: see [Direct reconciler](#direct-reconciler)):

1. **Remove the workload Applications.** The unit publishes a tree whose
   `_nyl/catalog` still contains the catalog Application and the generated
   AppProjects but no workload Applications; the workload trees stay. In a
   shared catalog, the target's catalog directory keeps its AppProjects and
   the shared manifest is untouched. The catalog is never empty, so Argo CD's
   empty-tree guard and the catalog's own self-prune policy never come into
   play, and the AppProjects outlive the Applications that use them. This commit is the teardown's `published`
   evidence. Argo CD's catalog sync prunes the workload Applications, and
   their finalizers delete the workloads.
2. **Remove the rest.** Once phase 1 counts as complete under `teardownWait`,
   the unit removes the remaining files it owns: exactly the files recorded in
   `_nyl/index.json`, which includes the workload trees and the catalog.
   Unowned files in the prefix, such as state files committed by other tools,
   are preserved, as in every reconciliation.

Implementation notes (reading a reconciler): the driver observes through one
seam that no reconciler's types cross. A
`nyl_units::kubernetes_publication::reconciler::ReconcilerReader` is given the
deployments and anchors a target expects (`nyl_render::gitops::observation`:
`ExpectedDeployment`, `Anchor`, recorded under `/reconciler`) and returns a
neutral `Reading`: per deployment its running revision (`Exact(commit)`,
`Mixed(reason)`, or none), health (`Healthy`, `Progressing`, `Degraded`, or
`Unknown`), and live source; per anchor whether it is visible; and, for each
expected deployment the reading lacks, whether the credential could have seen
it. One `reconciler::evaluate` owns the rules over those types: `accepted`
and `healthy` (`evaluate`), and the answer of a teardown wait (`removal`),
both refusing to conclude from what is not provably visible. Argo CD's two
readers (the Kubernetes API of the control-plane Cluster, with the anchors as
its proof of absence, and the Argo CD API server, with `can-i` per
Application) map into the reading in `kubernetes_publication::argocd`, and the
Argo CD renderer in `nyl_render::gitops::observation` derives the expected
deployments from the generated Applications. A second reconciler kind adds a
variant of `ReconcilerLocation` with its renderer next to Argo CD's, and a
reader implementing `ReconcilerReader` plus the one match arm of
`reconciler::reader` that builds it; `evaluate`, the driver's observation, the
orchestration engine, state, and the CLI do not change (the
[direct reconciler](#direct-reconciler) is that second kind). The facts recorded at
resolution follow the same split: the neutral part (`kind`, `publication`,
`expected`, `anchors`) and the kind's location (for Argo CD `instance`,
`namespace`, `cluster`, `context`, and `access`).

As implemented: the `KubernetesPublication` driver
(`nyl_units::kubernetes_publication`) carries out both phases through Git on
the real publication path (the tree changes are in
`nyl_render::gitops::publication`), records the phase 1 commit as the wait's
`phase1Commit`, and removes in phase 2 exactly the files of its own
`_nyl/index.json`. A publication that loses the race for the branch head,
before or at its compare-and-swap push, is built again on the new head, at
most three times and within the unit's `timeout`. A target with
`fromPublication` bindings is rendered again on the new head: when the state
it read (the `@input/`, `@publication/`, and `@carried/` digests of its
index) is unchanged, it publishes there; when it changed, the execution ends
without publishing as a retryable failure with `NYL-KUBERNETES-STATE-CHANGED`,
and the next run resolves the unit again with the new state. A file the target
owns that changed outside Nyl fails the execution, as reconciliation does.
Any other failure of the push may have published and ends the call. Drift
detection renders again and compares the branch with the tree: a difference
or an owned file modified outside Nyl is `drifted`, and a comparison that
cannot be made is `unknown`. The driver records the target's effective teardown settings at
resolution (see Teardown readiness below). A unit that declares `observe: {}`
also records, at resolution, the reconciler facts its readings need
(`reconciler`: the kind of reconciler that runs the target and its instance,
how it is read, the publication destination, the deployments the target
expects of it, and the anchors that show a reading comes from that
reconciler; an authored `reconciler` is rejected), so `attest` and the
teardown wait read the stored document. Only the render-derived part keys the
unit (the reconciler's identity, the publication's branch and prefix, the
expected deployments, and the anchors); how the reconciler is read, its
`access`, local `context`, and the publication URL, is recorded but never
keys it. The strategies `delay`, `manual`, and `observe` work
as the table below says. Inline targets,
the owner in the ownership index, and owner fencing work as described above, in
both phases of the teardown. Tier-1 tests keep the test kind
`FakePublication` (scripted outcomes, the fake Argo CD observer
`FakeObserver` for the `observe` wait, accepting inline targets and evaluating
scenario-declared teardown settings with the real driver's readiness rules).

`teardownWait` decides when phase 1 counts as complete. The same point gates
phase 2 and the teardown of the units the publication depends on, such as the
IAM roles, subnets, or databases its workloads use, and through the reverse
dependency order the units those depend on in turn. Units with no dependency
path to the publication are not held back by its wait (see
[Deletion](#deletion)):

| Strategy | Phase 1 completes when | Default for |
| --- | --- | --- |
| `observe` | The driver reports that Argo CD no longer has any of the target's workload Applications (every Application except the catalog's), within the unit's `timeout`. Otherwise the teardown is uncertain, with the reason `applications-remain` (listing them) or `observation-failed` (with the driver's error, such as a missing credential or an unreachable Argo CD) | a unit that declares `observe` |
| `delay` | `duration` has passed since the phase 1 commit, such as `{strategy: delay, duration: 10m}`; the teardown is recorded as not observed | |
| `manual` | An operator confirms with `nyl teardown -e <env> --unit <u> --confirm-removed`; until then the unit is `tearing-down` and its dependencies wait, exit 2 | a unit without `observe` |

`observe` reads the reconciler as a unit with `observe: {}` does (see [Reading Argo CD](promotion.md#reading-argo-cd)):
each poll the shell asks the unit's driver, `Driver::observe_removal`, which
reads the deployments its stored document records (for Argo CD, the target's
Applications), with the credentials the unit admits; the shell interprets no
reconciler answer. Removal is concluded only from what the reading proves
visible: a deployment the credential may not see, or a reading that does not
show what only the reconciler running this target has, ends the wait with
`observation-failed` instead of counting the deployment as removed, so the
token or role needs `get` on the target's Applications.
Two escape hatches end any wait, whatever the strategy, once phase 1 is
published and the wait is pending or uncertain. Both are needed because a
`deleting` unit is torn down from its stored desired document, so changing
`teardownWait` in source cannot rescue it:

- `nyl teardown -e <env> --unit <u> --confirm-removed [--reason <text>]` is
  the operator's assertion that the workloads are gone, for example after an
  `observation-failed` condition and a manual check. It clears the condition,
  runs phase 2, and lets the dependencies' teardown proceed. The transition
  commit records who confirmed (`Nyl-Requested-By`), the reason, and that
  completion was confirmed by an operator rather than observed.
- `--allow-incomplete` proceeds without any claim: phase 2 and the
  dependencies' teardown run at once, and the transition commit records the
  teardown under `details.incomplete` with what may remain, reported with
  `NYL-TEARDOWN-INCOMPLETE`, so state never claims a confirmed removal.

`status` suggests both for an uncertain wait: fix the observer configuration
and retry, or confirm after checking by hand.

A confirmation can also end a wait that a run is still sitting in, such as a
CI job's `state delete --teardown` waiting out a `delay`, or an `observe` wait
whose observer lags. That run holds the environment's lease, so the
confirmation travels through a signal ref instead of a second run. The same
ref carries `nyl hold` to a running run (see [Deletion](#deletion)); each
`Signal` names its `action`, `confirm-removed` or `hold`
(`nyl_orchestration::shell::signal` delivers both, and
`rules::teardown_wait` decides what a confirmation ends):

- `--confirm-removed` looks at the lease first. With no lease held, it
  confirms directly as above the wait recorded in the unit's observed
  state, and a unit without one is rejected before anything runs; with
  `--all` it confirms every recorded wait. It never confirms a wait that
  its own run's phase 1 starts, which waits under its strategy like any
  other (owner: `nyl_orchestration::shell::teardown`). When the lease holder is waiting on that unit's
  teardown, it pushes a `Signal` to the run's signal ref,
  `refs/nyl/<env>/signals/<run-id>`, naming the operator, the reason, the time, the
  run ID, and the phase 1 commit it confirms. It reports where the
  confirmation went with `NYL-CONFIRMATION-DELIVERED`, such as
  `confirmation delivered to run 0b8f… (https://ci.example.com/runs/1234)`,
  and exits 0. When the lease holder is doing anything else, it exits 2 as for
  any held lease. After pushing the
  signal, it checks that the lease still belongs to that run; if the run
  finished in the meantime, it deletes its signal and confirms directly.
- A waiting run checks its signal ref about every 15 seconds. A confirmation
  for the unit and phase 1 commit it is waiting on ends the wait early: phase 2
  and the dependencies' teardown continue in the same run, and its transition
  commit records who confirmed, from where, and that removal was confirmed
  rather than waited out or observed.
- A confirmation is bound to its run ID and phase 1 commit, so a leftover
  signal can never end another teardown's wait. A confirmation for a unit no
  run or recorded wait is waiting on is rejected when it is sent, with
  `NYL-CONFIRMATION-REJECTED` (exit 1).
- Without a confirmation, the wait ends as its strategy says: a `delay` runs
  out and is recorded as not observed.
- When the waiting run's stdin is a terminal, it shows the remaining time and
  accepts Enter as a confirmation from the person running it, recorded the
  same way.
- A `manual` wait never keeps a run waiting, because the run exits 2 at once;
  a later `--confirm-removed` applies directly.
- A wait that goes on beyond the run is reported with
  `NYL-TEARDOWN-WAITING`: on the waiting publication, `tearing-down` with
  why it waits (the run, owner `nyl_orchestration::shell::run::teardown`,
  and `status` from the recorded wait, owner `rules::unit_state`), and on
  each unit whose teardown waits for it, naming the publication
  (`rules::teardown`). The run exits 2. The fix names the next
  teardown-capable run, which continues the wait, and `--confirm-removed`.
- A recorded wait persists between runs in the unit's observed document under
  `waiting` (phase, reason, `since`, the advisory `until`, `phase1Commit`, and
  the run that published phase 1), so a later run, `--confirm-removed`, or
  `--allow-incomplete` continues it without publishing phase 1 again. The
  wait's end is computed from the recorded `since` and the strategy
  (`rules::teardown_wait::ends`); a driver's `until` is advisory.

Teardown readiness is checked statically from the target's effective
settings, as recorded in the unit's stored desired document, so `status`,
`plan`, and a teardown agree without rendering. Resolution renders the target
to compute the execution key; the driver reads the settings from that render
(the catalog Application and workload Applications as Argo CD will see them,
and the Namespaces the tree owns, with their `Prune` and `Delete` sync options) and records them in the resolved spec under
`teardownSettings` through `KeyInputs.recorded`, so an authored
`teardownSettings` is rejected. The driver reads them back through
`Driver::teardown_readiness`, which evaluates them itself: the table below is
the publication driver's (`nyl_units::kubernetes_publication::readiness`),
and the shell only coordinates and displays the findings (their codes, the
setting that fixes each, and the split into blocking requirements and
remnants). A document resolved before the settings were recorded has none and
no requirements. A tree without a catalog Application leaves the catalog to a
parent Application, so its catalog rows are `external: not checked`:

| Requirement | Setting | If unmet |
| --- | --- | --- |
| The catalog prunes removed workload Applications on its own | catalog `syncPolicy.automated` enabled with `prune: true` | Blocking: the Applications stay until someone syncs the catalog with pruning |
| Deleting an Application deletes its workloads | `applicationDeletionPolicy` of the catalog Application and of every workload Application is `Foreground` or `Background`, not `Orphan` (a workload Application without a finalizer makes the recorded policy `Orphan`, because the catalog's own finalizer does not delete the workloads of an Application it prunes) | Blocking: workloads keep running without an Application |
| Owned Namespaces do not stop deletion | namespace `prunePolicy` and `deletePolicy` are `Automatic`, or `deletePolicy` is `Retain`; either one left at the default `Confirm` renders `Prune=confirm` or `Delete=confirm`, and Argo CD holds the Application's finalizer while a resource it would delete carries either, until someone confirms. `Delete=false` keeps the Namespace, so nothing is confirmed; `Prune=false` only affects syncs. Read from the rendered sync-options annotation, so templates and overrides count | Blocking: the Application and its workloads stay until the deletion is confirmed in Argo CD |
| Owned Namespaces are removed | namespace `deletePolicy` is `Automatic` | Remnant: empty Namespaces remain |
| The catalog Application and AppProjects are removed | the target's catalog is owned by a parent (see Catalog ownership below) | Remnant: a self-managing catalog Application and its AppProjects remain after phase 2 |

The three blocking requirements decide whether teardown can succeed; the
remnant rows leave objects behind.

**Catalog ownership.** Something must sync each target's `_nyl/catalog` into
Argo CD. Nyl leaves the choice to the project:

| Setup | Target setting | Created by | Removed by |
| --- | --- | --- | --- |
| Self-managing catalog per target (default) | `catalogApplication.enabled: true` | An operator applies the generated catalog Application once; it then manages itself | Nothing: after phase 2 the catalog Application and AppProjects remain |
| One self-managing catalog shared by many targets, such as every preview on a branch | `catalogApplication.shared: {name}` | An operator applies the shared catalog Application once; it then manages itself, and each target's objects appear in it on the target's first publish | The shared catalog prunes each target's objects as phase 1 and phase 2 remove them; the catalog itself stays until the shared catalog is decommissioned |
| One parent Application the project writes itself | `catalogApplication.enabled: false` | The parent, applied once, syncs `*/_nyl/catalog/*` from the branch | The parent prunes each target's objects as phase 1 and phase 2 remove them |
| An ApplicationSet generating one catalog Application per target | `catalogApplication.enabled: false` | The ApplicationSet, when `<prefix>/_nyl/catalog` appears | The ApplicationSet, when phase 2 removes it |

A shared catalog is the recommended setup for previews: one Argo CD
Application manages every instance, instead of one catalog Application per
instance.

```text
previews branch
  _nyl/shared/previews/catalog.yaml     # the shared catalog Application, managing itself
  _nyl/shared/previews/_nyl/index.json  # its ownership index, owner catalog:previews
  pr-123/_nyl/catalog/…                 # pr-123's Applications and AppProjects, as for any target
  pr-123/web/…                          # pr-123's workload trees
```

- Each target stays an ordinary target in its own prefix, with today's catalog
  layout, index, and reconciliation. `shared` implies
  `catalogApplication.enabled: false` for the target, so its prefix contains
  no catalog Application, only its Applications and AppProjects. Targets
  sharing a catalog must publish to the same repository and revision, and
  that revision holds nothing else: validation rejects a target that does not
  share the catalog, or shares another one, on a repository revision that
  carries a shared catalog, because the catalog's include pattern would sync
  its catalog directory too and give its Applications two owners.
- The shared catalog Application sources the branch root recursively with
  `directory.include: '{_nyl/shared/<name>/*.yaml,*/_nyl/catalog/*}'`, so it
  syncs its own manifest and every target's catalog directory, and nothing
  else. Argo CD matches `include` against each file's path relative to the
  source path with gobwas globs compiled without separators, so `*` also
  matches across `/`; this holds for Argo CD 2.8 through current releases.
- The `shared` block takes the name and the settings of the catalog
  Application (`project`, `syncPolicy`, `applicationDeletionPolicy`,
  `selfPrunePolicy`, `labels`, `annotations`), which the target's own
  `catalogApplication` fields would otherwise carry; a target that declares
  both is invalid, and so is a shared catalog without a `pathPrefix`. Targets
  that name one catalog render the same manifest (the shared block, but also the
  ArgoCDInstance, its catalog defaults and destination, and the repository),
  because one manifest is rendered for all of them. The checks that span targets fail with
  `NYL-KUBERNETES-SHARED-CATALOG-CONFLICT`, naming the targets: another
  manifest for the catalog, another repository or revision for it, another
  or no catalog on a revision that carries one, a catalog name that is also
  a target's own catalog Application, and a target prefix that is, contains,
  or lies beneath the reserved prefix `_nyl/shared`.
- The shared manifest lives under the reserved prefix `_nyl/shared/<name>/`
  with its own ownership index, a version 3 index whose `owner` is the string
  `catalog:<name>` (a unit incarnation is an object, see the Kubernetes
  publication unit), so no target ever writes outside its own prefix. Nyl
  renders it from the `shared` block and the ArgoCDInstance's catalog
  defaults; the index records no source commit, so two targets that share the
  catalog render byte-identical trees. `catalog:<name>` is no unit
  incarnation that a teardown could remove, so no unit takes the prefix over
  (`NYL-KUBERNETES-OWNER-FENCED`), and the catalog's publication refuses a
  prefix whose index names another owner.
- The manifest has one writer, and a target's publication only creates it:
  when the publication branch holds no tree at the catalog's prefix, the
  publication of a target that names it publishes the tree first, as its own
  commit, by compare-and-swap (a publication that loses the race for the branch
  head reads the new head again, and stops as `current` or `differs` once
  another has created the manifest); when the branch holds the same manifest,
  nothing happens; when it holds another, a publication leaves it alone, so pull
  requests running different Nyl versions never overwrite each other. It records
  the result (`created`, `current`, or `differs`) as `sharedCatalog` in the
  `PublishedTree` artifact it records, and for `differs` returns the warning
  `NYL-KUBERNETES-SHARED-CATALOG-DIFFERS` (fix: the writer below) through the
  driver's `warnings`, which the shell records with the receipt and shows in the
  run report, `plan`, and `status` until the unit executes again. That is what
  the unit's last execution found, not the branch's state now: `nyl build <unit>
  -e <env> -- --shared-catalog --diff` reads the branch. The writer
  that replaces it is the build of any unit
  that names the catalog, at the revision whose Nyl version and declaration
  should win, usually the default branch's from a scheduled job:
  `nyl build <unit> -e <instance> -- --shared-catalog --push` (see
  [Direct builds](#direct-builds)). The build renders the unit's whole target,
  so it needs a live instance (or declared environment) whose references
  resolve; with no live instance nothing uses the catalog and no writer is
  needed, because the first instance's publication creates it. The scheduled
  job that reconciles the fleet runs the writer after it. The same arguments without `--push` print
  the rendered tree, and with `--diff` compare it with the branch. No
  environment owns the shared catalog, so that push is never refused as
  `NYL-KUBERNETES-BUILD-TARGET-OWNED`.
- Nothing is reconciled from state: Git holds the other targets' files, and
  a publish removes only files in the publishing target's index.
- Nyl renders the shared catalog, so the readiness table checks its settings,
  read from the shared Application the branch holds, at resolution and again
  when a teardown starts, not from the manifest the instance renders. The
  reading is recorded in the unit's document as a member that is never keyed
  (the key follows what the target renders, so creating the manifest or a
  writer's push never re-executes a preview), and a teardown over a catalog
  that no longer prunes, or whose Application cannot be read, is held by the
  driver with the readiness code even for an instance that expired and is
  removed from its stored document: Argo CD runs what is published, so a pull
  request that declares a pruning catalog over one that does not prune reports
  the published settings, and its teardown is held. A branch without the
  manifest yet has nothing that prunes, so the catalog rows are not checked
  until it exists, and a branch that cannot be read, or a manifest that is no
  Application, is recorded as a blocking row (an offline run of `plan` shows it
  too, because resolution reads the branch and has no cached fallback). Whether it prunes, and its deletion policy, are the rows. The shared catalog prunes each target's objects as phase 1 and
  phase 2 remove them, so the target leaves no catalog remnant
  (`NYL-TEARDOWN-CATALOG-REMAINS` does not apply).
  With `enabled: false`, Nyl cannot see the parent's sync and prune settings:
  the catalog rows become `external: not checked`, which the preflight reports
  without blocking, and `observe: {}` still checks that the Applications
  actually disappear.
- A branch carries one shared catalog, dedicated to it: every other
  `_nyl/shared/<other>/` tree on it refuses a publication or build with
  `NYL-KUBERNETES-SHARED-CATALOG-CONFLICT`, because each self-managing catalog
  syncs every target's catalog directory. Renaming or decommissioning a
  catalog is a procedure, not an edit: delete its Argo CD Application with the
  resources kept (non-cascading, so the targets' Applications survive), remove
  `_nyl/shared/<old>/` in one commit (the files its index owns, then its
  index, or release the prefix as for any owner), and only then publish the new
  one; the old catalog's decommissioning with its resources would otherwise
  cascade through every target. The branch also holds one project's targets:
  a catalog's include pattern and sync are the same for everything on it.
  One shared Application is one sync for every preview on the branch: a
  manifest Argo CD rejects blocks all of them, and an `observe` wait reads
  nothing past it and fails closed.
- Phase 1 keeps the target's AppProjects and publishes the target's catalog
  directory without the workload Applications, and touches nothing outside the
  target's prefix. A target with a shared catalog generates no catalog
  Application, so the teardown wait's anchor is the same as for
  `enabled: false`: the AppProjects its Applications belong to. The shared
  catalog Application is no anchor of the target's, because it sources the
  whole branch and matches no part of the target's publication; a target whose
  only project is `default`, or whose projects are not visible, fails closed as
  it does without a shared catalog (see
  [Reading Argo CD](promotion.md#reading-argo-cd)).

Nyl reports unmet requirements where they matter:

- `nyl plan` and `nyl reconcile` warn for every `KubernetesPublication` whose
  `deletionPolicy` is `Teardown`, including every unit of a template instance,
  naming each unmet requirement and the setting that fixes it.
- `nyl state init --template` warns when the template contains publication
  units that are not teardown-ready, because instances are always torn down,
  and when a unit's teardown would wait for an operator that an unattended
  removal, such as an expiry, lacks (`teardownWait: manual`,
  `NYL-TEARDOWN-WAIT-MANUAL`). The units render at the entry checkout and
  resolve against the instance's state like a `plan`, so a unit that waits for
  the outputs of another cannot show its requirements before its producers
  ran, and `plan` and `reconcile` show them from then on. A driver reports what
  an unattended removal would wait for in `TeardownReadiness::unattended`,
  which never blocks.
- `nyl get units -e <env> -o wide` and `nyl status` show teardown readiness per
  publication unit (`ready`, or `incomplete: <reasons>`).
- Every teardown runs a preflight listing what would remain, however it was
  requested: omission during `reconcile`, expiry, `teardown --unit`,
  `teardown --all`, and `state delete --teardown`. When a blocking requirement
  is unmet, the teardown does not start unless the invocation passes
  `--allow-incomplete`; the unit is reported as pending teardown with the
  reasons and `NYL-TEARDOWN-INCOMPLETE`, exit 2. A teardown that proceeds
  under `--allow-incomplete`, or ends a wait without a claim of removal, is
  recorded in the run's transition commit under `details.incomplete`, listing
  per unit what may remain, so state never claims a clean teardown that did
  not happen. The blocking rows raise `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`,
  `NYL-TEARDOWN-APPLICATIONS-ORPHANED`, and `NYL-TEARDOWN-NAMESPACE-CONFIRM`;
  the remnant rows warn with `NYL-TEARDOWN-NAMESPACES-REMAIN` and
  `NYL-TEARDOWN-CATALOG-REMAINS`; each names the setting that fixes it.
- `nyl validate` does not check teardown readiness, because it cannot know
  whether a target will ever be torn down.

### Direct reconciler

A DeploymentTarget is delivered one of two ways. By default an external
reconciler, Argo CD, runs the published tree. With `reconciler: {direct: {}}`
Nyl itself applies the target's Releases to the target's Cluster with the
mechanics of `nyl apply` (server-side apply, pruning by release state, the
`Prune` and `Delete` sync options), and still publishes the tree. The choice is
the target's, never the unit's: a `KubernetesPublication` places a target and
does not know which reconciler runs it, `observe: {}` keeps meaning "also read
what the reconciler did", and the same declaration, scenario, and promotion
evidence work for both. The invariant the design keeps is that **Git is never
behind the cluster**: the cluster runs only what a published commit holds, so
a failure between the two effects leaves Git ahead, which the next execution
converges, never the cluster running bytes no one recorded.

#### Declaration

```yaml
apiVersion: k8s.gitops.nyl/v1
kind: DeploymentTarget
metadata: {name: dev}
spec:
  clusterRef: {name: kind-dev}     # the Cluster declares destination and live.context
  reconciler:
    direct: {}                     # absent: Argo CD, as today
  publication: {repositoryRef: {name: deploy}, revision: deploy, pathPrefix: targets/dev}
  applicationGroupSelector: {matchLabels: {tier: platform}}
```

- `spec.reconciler` is optional, an object with exactly one member, and today
  the member is `direct`, written `direct: {}`. A tagged one-member object,
  not a bare keyword or an alternative to `argocdRef`, because a reconciler
  kind may need options (`direct` takes none yet) and a kind that is not Argo
  CD must not borrow Argo CD's reference field. Absence selects Argo CD, so
  every released declaration keeps its meaning.
- **Exclusive and validated.** `DeploymentTarget::validate` rejects
  `reconciler.direct` together with `argocdRef` and with any
  `catalogApplication` field other than its defaults (`enabled`, `shared`,
  `name`, `project`, `syncPolicy`, `applicationDeletionPolicy`,
  `selfPrunePolicy`, `labels`, `annotations`): a direct target generates no
  Application, catalog, or AppProject, so each would be a setting that does
  nothing. An empty `reconciler` object, or one with an unknown member, is
  rejected. The same declaration is valid in an inline target
  (`spec.target.inline`).
- **What a direct target still needs.** The Cluster, with its destination and
  committed Kubernetes capabilities exactly as for Argo CD (rendering is
  unchanged), and a publication, because the tree is still published (see
  below). A unit that applies or reads a direct target also needs the
  Cluster's `spec.live.context`: resolution fails before effects with
  `NYL-KUBERNETES-CLUSTER-CONTEXT-MISSING`, whose fix names the Cluster and
  the field. Rendering and `publish-tree` never need it.
- **What it no longer needs.** No ArgoCDInstance (explicit or implicit), no
  catalog Application, no AppProject, no Application manifests, and no
  Application names. `ApplicationGroup` fields that describe Argo CD
  (`applicationNamespace`, `projectRef`, `projectTemplate`, the `retained*`
  lists, `syncPolicy`, `applicationDeletionPolicy`, `applicationNameTemplate`,
  `releaseCustomization`, and the Application `labels` and `annotations`)
  are ignored for a direct target, because one group may be selected by
  targets of both kinds; `destinationNamespace`, `outputPath`, `namespace`,
  and `sharedNamespaces` apply to both.
- **ArgoCDInstance rules.** "An explicit ArgoCDInstance makes `argocdRef`
  mandatory for every target" becomes "...for every Argo CD target", and the
  implicit target-local instance exists only for an Argo CD target. A project
  of direct targets declares no ArgoCDInstance, and a project with explicit
  instances sets `argocdRef` on its Argo CD targets alone. The seam is
  `resolve_argocd_instance` in the target compiler: a direct target resolves a
  `Delivery::Direct` value instead, so the compiler, the cache key, and
  `RenderedTarget` carry `delivery: Delivery` (`ArgoCd(ArgoCdLocation)` or
  `Direct(DirectLocation)`) in place of today's mandatory `argocd`.
- **ApplicationGroup projects.** A direct target generates no AppProject, so
  project derivation (and the rule that deriving a whitelist fails when a
  kind's scope is unknown) is skipped. What does not depend on Argo CD still
  holds for every target: a Release's resources stay within the namespaces it
  may use, a Namespace is managed by the group's `namespace` policy, and a
  rendered duplicate object is rejected. Nothing at runtime enforces the
  project-style cluster-resource whitelist, so a direct target relies on the
  render-time checks and on the credentials' own RBAC.
- **Namespace ownership.** A destination or additional Namespace belongs to
  its owning Release exactly as it belongs to its owning workload Application
  today, and the Namespace's `Prune` and `Delete` sync options (from `namespace.prunePolicy`
  and `deletePolicy`) mean what `nyl apply` makes of them. Bootstrap
  namespaces stay externally owned unless `sharedNamespaces` delegates them.
  A `Dedicated` shared-namespace owner would generate an Application with no
  Release to hold release state, so it is rejected for a direct target, with
  the fix to own the Namespace by a Release or declare it `External`.
- **Collisions.** Release state is stored under the Release's name in the
  Release's namespace of the target's Cluster, so two Releases of one direct
  target, or of two direct targets that reach the same cluster, may not share
  namespace and name: validation rejects it with
  `NYL-KUBERNETES-DIRECT-RELEASE-COLLISION`, across every target it can see
  (static, inline, and the sample instances of every EnvironmentTemplate, as
  the Argo CD name check does). "The same cluster" is the same declared
  destination (`server`, or `name`), not the same Cluster resource, so two
  Cluster resources that point at one cluster, such as one with a live context
  for direct applies, collide as one. The rendered-duplicate check (one
  workload resource has one owner) extends the same way: an object rendered by
  Releases of two direct targets on one destination is rejected, in the
  validation that sees both. At run time, the rule that keeps one Release from
  deleting what another still tracks is the purge exclusion below. The
  generated Application-name uniqueness rule does not apply to direct targets.

Every entry point takes the path the declaration selects:

| Entry point | For a direct target |
| --- | --- |
| `nyl render`, `nyl build` | Unchanged: render the Release or the tree, never connect. |
| `nyl render-tree`, `nyl publish-tree` | The tree of Releases and the ownership index, without `_nyl/catalog`. Publication only: nothing reaches the cluster. The index records the reconciler (see below). |
| `nyl diff`, `nyl apply -f <release> --target <t>` | The Release is taken from the compiled target (`gitops::direct`), applied by the shared function below with the CLI's options (`--concurrency`, `--append-release`, `--confirm-prune`, `--handoff`, `--context`). `nyl diff` reads and is never refused; `nyl apply` of a Release an environment owns is refused (see Fencing). The context is `--context`, else the Cluster's declared `live.context`, never the current context once the Cluster declares one. Without either, the current context is used only when the Cluster's destination server can be verified against it (a concrete URL); an in-cluster or by-name destination requires `--context`. The server of the context must match the declared destination either way. |
| `nyl reconcile` | The unit's execution: publish, then apply every Release (below). |
| `nyl attest`, `nyl observe`, teardown wait | Read through the direct reader (below). |
| `nyl detect-drift` | The branch compared with the tree, as for Argo CD. Reading the cluster for drift (a release revision that runs another publication, live edits) is deferred; `nyl diff` shows live differences, and `nyl attest` shows what runs. |
| `nyl validate` | The static rules above, including collisions. |

The orchestrated and the manual path share the apply code and its fence, and
differ only where a user chooses: the manual `nyl apply` takes its context and
kubeconfig from its flags and the environment, as a local command does, and
the driver only from the admitted environment and the declared context.

#### Publication and order

A direct target still publishes its tree to Git. The published tree is the
receipt, the provenance record, and the input promotion reads, and it is what
the release revisions in the cluster cite. It holds:

- the Release directories, byte for byte what Argo CD would sync from them
  (the same files `nyl apply` reads from the compiled target);
- `_nyl/index.json`, a version 3 index with the owner, the compilation
  metadata, and a new optional member `reconciler` (`direct`; absent means
  Argo CD). `reconciler` is part of the index's identity (`same_owner`), so a
  publication whose target changed reconciler is refused, whichever command
  makes it, with `NYL-TREE-RECONCILER-CHANGED` and the fix. The refusal is
  essential, not tidy: moving an Argo CD target to direct removes
  `_nyl/catalog`, and a self-managing catalog that prunes would then delete
  the Applications and, through their finalizers, the workloads. Switching is
  refused in this version, never an edit. The unit adds
  `NYL-KUBERNETES-RECONCILER-CHANGED`, retryable, whose fix is to restore the
  declaration or replace the unit with `nyl teardown -e <env> --unit <u>`,
  before any effect. The receipt's `PublishedTree` now records `reconciler` and
  `cluster` beside `target`, and `NYL-KUBERNETES-TARGET-CHANGED` compares the
  Cluster as well, because the old Cluster would keep the workloads. The
  non-destructive way from Argo CD to direct, for workloads that must keep
  running, is a documented procedure: delete the catalog and workload
  Applications with their resources orphaned (`kubectl delete application
  --cascade=orphan`), tear the old unit down with `nyl state forget` (or start
  a new unit), then publish and apply under the direct declaration, whose
  forced server-side apply takes the objects over. The reverse
  (`nyl release delete --no-purge`, then publish for Argo CD) follows the same
  shape.

It holds no Application, AppProject, catalog, or shared-catalog manifest.

**Order.** A reconcile of a direct unit runs: render; preflight the cluster
(connect through the admitted `KUBECONFIG` and the declared context, check the
server against the destination, read the release state and fence of every
Release, all read-only); publish the tree (compare-and-swap, with the owner
fence and retries of every publication); apply each Release; once every
Release reached `Deployed`, purge the Releases that left the tree; return the
`PublishedTree` receipt. Git comes
first because the release revision records the publication commit, so the
running revision is exact; because the cheap, undoable checks (owner fence,
head race, validation) all precede any cluster effect; and because it keeps
the invariant above. Applying first would let the cluster run bytes whose
push then failed, with no record of what ran.

| Where it stops | What happened | Outcome | Next execution |
| --- | --- | --- | --- |
| Render, preflight, credentials, context, destination mismatch, Git owner fence, release fence | Nothing | `Failed` (`credentials` or `ownership`, retryable) or a failure before effects | Retries |
| The push may or may not have landed | The branch may hold the tree | `Uncertain`, as for every publication | Converges: publishing again is a no-op or a retry |
| One or more resources rejected (admission, validation, API error) | Git ahead; the Release's revision records `Failed` with the keys it applied; other Releases may have applied. Neither the purge of vanished Releases nor the receipt happens | `Failed` (`tool-error`), naming each Release and resource; `retryable` unless every failure is the API rejecting the manifest itself | Applies every Release again as a new revision; the tree is unchanged |
| The connection drops mid-apply, or the revision cannot be recorded | Unknown | `Uncertain` | `RecoveryPolicy::Converge`: applying again is safe |
| The unit's deadline passes | Partial; the revision is recorded `Failed` | `Failed` (`timeout`) | As above |
| A prune fails | Applied; the keys that failed to delete stay recorded in the revision | `Failed` (`tool-error`, retryable) | The next apply retries the delete |

A receipt is written only after every Release applied and pruned, and Releases
that left the tree are purged only then. Objects
that a `Prune=confirm` Namespace policy keeps are not failures: the
driver never confirms (confirmation is an input outside the declaration), keeps
them tracked, and returns the warning `NYL-KUBERNETES-PRUNE-AWAITING-CONFIRMATION`
naming them, with the fix of declaring `namespace.prunePolicy: Automatic` in
Git. Both recovery policies are `Converge`; `inspect` stays unsupported.

#### Applying

`nyl apply` and the driver share one implementation in `nyl-render`. Today its
mechanics live in the CLI (`nyl/src/cli/commands/apply.rs`: the release
recorder, the batched apply, pruning, the purge of `release delete`), which
the driver cannot call and which the architecture says should hold no business
rule. They move to `nyl_render::kubernetes::lifecycle`, and the CLI keeps
only argument parsing and the renderer of `ApplyEvent`s. `nyl apply` behaves
as before, with three deliberate changes: `release delete` purges with the
shared prune executor (reverse apply order with waits for termination, where it
deleted in a plain loop), so the one rule serves both; the second Ctrl-C's
`process::exit(130)` leaves library code and becomes part of the interruption
parameter below; and `nyl apply --target` resolves its context by the rule in
the entry-point table. Each is a changelog entry, with the owner refusal and
`NYL-KUBERNETES-TARGET-CHANGED` comparing the Cluster:

- `prepare_manifests` (namespace resolution from the destination namespace,
  duplicate collapse), `apply_release`, and `purge_release`, which carry
  `ReleaseRecorder` (`reserve`, `run`, `complete`), `apply_sorted_manifests`,
  `keys_to_prune`, `collect_live_state`, `filter_by_prune_option`,
  `prune_resources` with its termination waits, the history limit, and
  `ensure_namespace_exists`;
- the typed `ApplyEvent` and `ReleaseRun` report (the CLI's `Phase` and
  `PhaseTiming` stay output types; the run returns its durations);
- the interruption, today `tokio::signal::ctrl_c` and a second Ctrl-C that
  exits the process, becomes a parameter of `apply_release` (a future for the
  first interruption and one for the escalation): the CLI passes Ctrl-C and
  exits on the second, the driver passes the unit's deadline and no
  escalation;
- `verify_cluster_server` (a context's server must match the Cluster's
  declared destination, unless the destination is the in-cluster address or a
  name), moved from `commands/cluster.rs`;
- `release_manifests`, which reads a Release's documents from a tree's
  directory, shared with `gitops::direct`: the driver reads the bytes of the
  tree it pushed (the files the push consumed, never a fresh render), so what
  is applied is what the commit holds.

The shared function takes a connected `KubeClient` and `ReleaseStorage`, so
how a client is obtained stays each entry point's: the CLI as today, the driver
through the explicit `Connection` of the readers (the kubeconfig files the unit
admits as `KUBECONFIG` and the Cluster's declared `spec.live.context`, never
the current context, `~/.kube/config`, or in-cluster configuration; a missing
variable is a failure naming it and the `env` entry that admits it).
Server-side apply is as `nyl apply` makes it: force-applied as the field
manager `nyl` (a handoff to Argo CD is a manual option and has no meaning for
a direct target), resources in Argo CD's kind order, one concurrent batch per
kind with at most eight in flight, discovery refreshed after
CustomResourceDefinitions register kinds, release state recorded `Rendered` before the
cluster is touched, completed `Deployed` or `Failed`, then pruning by
release state (the most recent `Deployed` revision plus what failed revisions
applied) honoring each live object's `Prune` option, in reverse apply order
with waits for termination, and the project's `[release] history_limit`. The
driver's `ApplySettings` come from the declaration alone: `confirm_prune`
false, no `--append-release`, the history limit from `nyl.toml`.

**Several Releases.** Each Release is its own release (own name, own
revisions, own state), applied one at a time in waves so that a target is
deterministic and reversible:

1. Releases that render CustomResourceDefinitions or APIServices, because
   others may render the kinds they register;
2. every other Release.

Within a wave, Releases are ordered by group, then Release name. `nyl apply`
orders resources inside a Release by kind (Namespaces and CRDs first); the
waves extend that rule across Releases, and teardown runs them in reverse. A
failed Release does not stop the rest of its wave, but later waves do not
start, because they may need what it registers. A Release whose namespace is
owned by another Release (`sharedNamespaces`) creates a bare Namespace to hold
its state if the owner has not applied yet, as `nyl apply` does for any release
namespace, and the owner's later apply adopts it.

One absolute deadline bounds the whole apply: the unit's deadline less the time
the publication took, read from the injected clock. Each Release's apply is
bounded by what is left of it and none starts once it has passed, so a slow
Release cannot give the next one a fresh budget, and a run that times out
applies nothing further and fails as a retryable `timeout`.

Each Release is prepared (its namespaces resolved against the cluster) when its
wave comes, not before, because a namespaced custom resource without a
`metadata.namespace` cannot be resolved until the wave-0 Release that registers
its kind applied.

A Release's prune never deletes an object another Release of the run renders.
The driver collects, from the documents alone and before the first wave, every
identity an object of each Release can have once its kind's scope is known (the
cluster-scoped one, and the namespaced one in the namespace it names or the
Release's default), and gives each apply two sets. What a Release that applied
already renders is dropped from the Release that lost it without being deleted.
What a Release that has not applied renders (and may not, when a later wave
aborts or the deadline passes) is left in the cluster and stays tracked by the
Release that lost it, so no object is left that nothing tracks. Moving a
CustomResourceDefinition, a PersistentVolumeClaim, or a Secret from one Release
to another leaves it in place, whatever the apply order. Reading an object's `Prune` or `Delete` option
fails closed in every prune and purge: an object whose live state cannot be read
is reported as failed and never deleted, because the option that would have kept
it may be the very thing the failed read hid.

**Releases that left the tree.** A Release the tree no longer holds is
purged by its state identity (namespace, name), and only after every Release
of the run reached `Deployed`: a failed apply may be the very thing that
tracks an object a vanished Release also tracked, so nothing is deleted while
the run is incomplete, and the receipt is skipped with it. Which Releases left
comes from Git, so it needs no cluster-wide listing: the Releases of every
ownership index along the first-parent history of the publication branch from
the last receipt's `PublishedTree` commit to the head (bounded by the walk
limit of the shared matching; a deeper history is a failure, never a guess),
minus the identities of the new tree. Without a receipt, no execution of the
unit completed, so every publication up to the walk limit may be its own and the
walk covers them all, not just the newest. When the walk reaches the limit without
reaching its start (a receipt too far back, a receipt the branch no longer holds,
or none), it fails with `NYL-KUBERNETES-RELEASE-HISTORY-TOO-DEEP` rather than
guess. The span matters: a run that fails after
publishing a tree without a Release, before purging it, leaves the receipt at
the older commit, so the next run still finds it, whatever was published in
between. Before deleting, the purge subtracts every key tracked by the latest
revision of every current Release (an object that moved between Releases is
not deleted) and never deletes a Namespace that a current Release uses as its
release namespace, destination namespace, or additional namespace. A vanished
Release is purged only if its latest revision names this unit incarnation as
owner, or an incarnation of the same environment and unit that is gone; an
unowned or foreign one is left and reported with
`NYL-KUBERNETES-RELEASE-LEFT-BEHIND`, a warning, so a Release an apply by hand
took over stays until it is removed with `nyl release delete --take-over`. The
purge of vanished Releases and the teardown are one planning and one purge
(`classify_releases`, `execute_purge`): every Release of the set is classified
first, the Namespace of every Release's state is deferred, and the foreign
Releases' objects and Namespaces are protected, so the order in which Releases
are processed decides nothing. A purge deletes what the Release still has in the
cluster, which is what a prune
reads (the objects of the latest `Deployed` revision and of the `Failed`
revisions after it, never the union of every stored revision, so an object kept
by `Prune=false` and later untracked is not deleted), by each object's `Delete`
option, in reverse order with waits. Only when every resource went does it delete
state: first that of the Releases that track no deferred Namespace, then each
deferred Namespace (left, with a `NYL-KUBERNETES-RELEASE-LEFT-BEHIND` warning, if
release state that is not the purge's own remains in it), and last the state of
the Releases that tracked them, because that state is the only record of those
Namespaces and a failure before then must leave it for the retry. The apply's
absolute deadline bounds the purge too, and each teardown phase takes one of its
own. Argo CD does this through the catalog's pruning
and the Application finalizers; the direct reconciler does it itself.

#### Fencing and ownership in the cluster

The release state in the cluster (the `nyl.release.v1.<name>.<revision>`
Secrets) gains a metadata member, recorded in each revision it writes
(additive: a new key of the Secret, which readers that do not know it
ignore):

```yaml
meta:
  owner: {environment: dev, unit: kubernetes, uid: 91c3…}   # absent: applied outside orchestration
  publication:                     # the destination the revision was applied for; always recorded with --target
    repository: …, branch: deploy, pathPrefix: targets/dev
    commit: 7be1…                  # absent when applied by `nyl apply`
```

The owner uses the same `RenderIndexOwner` as the Git index, so one fence
answers both questions (`ExecutionContext::incarnations`, the `fence`
function of the driver). `ReleaseRecorder::reserve` creates the next revision
with a create that fails when it exists, so the revision number is a
compare-and-swap and the claim is the reservation itself: two executions
cannot both record revision N+1. The revision is numbered from the list of
revisions the fence decided on, not from a second listing, so a claimant that
created the next revision between the check and the create makes the create
fail instead of being skipped over.

| Latest revision | Orchestrated apply |
| --- | --- |
| No revision | Create it, owned by this incarnation |
| Owner is this incarnation | Proceed |
| Unowned, with a `publication` destination equal to this target's | Adopt: the new revision records the owner, as the first orchestrated publication adopts an unowned index |
| Unowned, with another destination, or none | `NYL-KUBERNETES-RELEASE-FENCED`: a Release of this name that this target did not apply lives here (a manual `nyl apply` with `--target` always records the destination, so only one applied without a target, or for another one, is refused) |
| A revision without `meta` after an owned one (an older `nyl` applied it) | Adopted with a warning, since the owner's earlier revision shows whose Release it is |
| Owner is another incarnation that is `gone` | Take it over (the new owner's apply prunes what its own revision no longer holds) |
| Owner is another incarnation that `exists` | `NYL-KUBERNETES-RELEASE-FENCED`: fenced until that unit's teardown completes or it is released with `nyl state forget` |
| Owner is another incarnation whose existence state cannot say | `NYL-KUBERNETES-RELEASE-OWNER-UNKNOWN`, failing closed |

A refusal is category `ownership`, retryable, and happens in the preflight,
before Git is touched, and again at `reserve` for the race. Two units that
reach one cluster's Release therefore fence like two units that reach one
published prefix, and the Git owner and the cluster owner must both pass.
Teardown and the purge of a vanished Release check the same owner.

`nyl apply` of a Release an environment owns is refused by `nyl apply`,
`nyl release rollback`, and `nyl release delete` with
`NYL-RELEASE-OWNED-BY-UNIT`, as `publish-tree` is refused over an owned
prefix, because a manual command cannot tell whether the unit still exists.
`nyl diff` only reads and is allowed. The deliberate way out is
`--take-over` on the same commands, which records no owner: the owner's next
reconcile adopts the Release again and reverts the manual change, so take-over
is for emergencies and adoption is not a persistence mechanism. A manual apply
of an unowned Release works as it does today, and `--target` records the
publication destination (repository, branch, prefix, without a commit). A
manual revision records no commit, so the reader below calls it unreadable
until the next reconcile applies the publication again.

Objects two different Releases both declare are rejected at validation when
the targets are visible together (see Collisions), and the purge exclusion
keeps a vanished Release from deleting an object a current one tracks; there
are no per-Release field managers, so server-side apply with `force` stays
last-writer-wins between Releases a validation did not see together.

#### Reading

`observe: {}` makes the unit attest `accepted` and `healthy` and read the
reconciler, which for a direct target is Nyl's own record in the cluster. A
`DirectReader` implements `ReconcilerReader`
(`nyl_units::kubernetes_publication::direct`), built by the one new match arm
of `reconciler::reader` from the recorded `Direct` location, over the explicit
`Connection`. `reconciler::evaluate`, the driver's observation, the
orchestration engine, and state do not change: every rule is expressible in
the neutral types.

The deployments are the target's Releases. For each, the reader lists the
Secrets labelled with the release name in the release namespace and decodes the
latest revision and the latest `Deployed` one. Only revisions whose
`meta.publication` (repository, branch, path prefix) equals the publication
the facts record count at all; the release state of another target or of a
manual apply that recorded another destination, or none, is unreadable to
this unit, so it can neither stand for this target's Release nor make the
anchor visible.

| Reading | From |
| --- | --- |
| **Present** | In a reading that concludes absence (a teardown wait), while a tracked object that its `Delete` option deletes still exists, other than a Namespace phase 1 defers (below); otherwise whenever the release has state. The meaning of "present" for a removal reading is the reader's, as for an Argo CD Application that exists until its finalizer has deleted the workloads |
| **Running revision** | Latest revision `Deployed`, with a recorded publication commit: `Exact(<commit>)`, which the shared matching resolves against the branch as it resolves an Argo CD revision (the directory tree and recorded inputs of the commit decide, not its identity). A latest revision with no commit (`nyl apply`) is `Unreadable` with the fix to reconcile |
| **Mixed** | Latest revision `Failed` or still `Rendered` (an interrupted or partly failed apply, which applied some of its resources): `Mixed {last: <commit of the latest Deployed revision>, reason}` |
| **Health** | The rollout status of the workload objects in the latest `Deployed` revision's manifest, below |
| **Source**, **Deleting** | None, false |

Health reads only workload objects that the revision's manifest declares (not
keys that stay tracked only because their prune awaits confirmation), with
`kubectl rollout status` semantics, folded `Degraded`, `Progressing`,
`Unknown`, `Healthy`, naming the worst object in `reported`:

| Kind | Healthy when | Progressing | Degraded |
| --- | --- | --- | --- |
| Deployment | Observed the current generation, updated and available replicas equal `spec.replicas`, no old replicas | Otherwise | `Progressing=False` with `ProgressDeadlineExceeded`; the object is missing |
| StatefulSet | Observed generation, ready and updated replicas equal replicas, current revision equals update revision (`OnDelete`: ready equals replicas) | Otherwise | Missing |
| DaemonSet | Observed generation, updated and available equal desired scheduled | Otherwise | Missing |
| Job | `Complete` | Running | `Failed`; missing, unless it sets `ttlSecondsAfterFinished` (then its disappearance after completion is expected and it is not read) |
| ReplicaSet without a controller owner | Ready equals replicas | Otherwise | Missing |
| Any other kind | Not read | | |

A paused Deployment reports `Unknown`. A tracked object that does not exist is
a finding; one the credential cannot read is an error of the whole reading,
never a finding.

**Visibility and absence.** The reader's proof is `Proof::Anchors`. A release
list that the credential may not make fails (HTTP 403) instead of returning
less, so an empty answer means the Release has no state of this target, and
the anchor `release-state` establishes that the reading comes from the cluster
Nyl applied to: it is visible when the connection's server was verified
against the Cluster's declared destination server (a concrete URL), which
proves the cluster itself, or else when at least one of the target's Releases
has a revision of this publication. A state Secret of this publication exists
only on a cluster where Nyl applied, and phase 1 of a teardown keeps it until
phase 2, so the anchor holds exactly when removal is concluded. A reading
with no anchor at all (no verifiable server and no state) is `Hidden` with the
reason: it was never applied, or it is another cluster, or phase 2 already
ran; `nyl attest` is unknown and a teardown wait `observation-failed`, whose
fix names `teardownWait: manual` with `--confirm-removed`, or `nyl state
forget`, when the state is known gone. Absent Releases are reported `Visible`
when the list answered. A credential that may not `get` a tracked workload
object makes the reading fail with `Forbidden` and the permission to grant,
never `Unknown` health. A context whose server differs from the declared one
fails the connection closed.

Failure modes follow the Argo CD table: an unreachable API, a rejected or
expired credential, or a forbidden read is `unknown` for `nyl attest` and
`observation-failed` for the wait; a partial reading (one namespace of
several fails) is unknown; the receipt's commit missing from the branch fails
both attestations as for Argo CD. Credentials are `KUBECONFIG` and the
declared context alone.

Without `waitHealthy`, `reconcile` attests nothing: Argo CD reconciles
asynchronously and direct apply synchronously, but both are read afterwards by
`nyl attest`, so the evidence path is one, and a job that needs `healthy` runs
`nyl attest` until it passes, with a timeout. With `waitHealthy`, the unit waits
for it inside the reconcile (see [Waiting for health](#waiting-for-health)).

#### Teardown

The teardown keeps the engine's two phases and `teardownWait`, with direct
meanings. It acts on what the stored document recorded, never on the current
declaration: the recorded reconciler kind (`/reconciler/kind`, which a direct
unit records whether or not it observes), the location, and the recorded
teardown settings decide how the unit is torn down. A declaration that now
renders another reconciler fails the teardown before effects with
`NYL-KUBERNETES-RECONCILER-CHANGED`, whose fix is to restore the declaration
(a teardown cannot be the remedy for itself). The engine hands every driver's
teardown the unit's stored artifacts, as it does a reconcile, so a driver can
tell what the unit published before, such as a tree's last commit; that is a
generic engine rule, with no kind-specific branch.

1. **Delete.** The unit publishes the tree without its Release directories
   (`without_workloads`: only `_nyl/index.json` stays, so the owner fence
   holds), then purges, in reverse wave order: every Release of the tree and
   every vanished Release the apply would purge (the same union), by
   `purge_release`, honoring each object's `Delete` option (`Delete=false`
   keeps it, `Delete=confirm` keeps it because the driver never confirms), in
   the exact reverse of the apply order (wave, then `<group>/<release>` key)
   with waits for termination. What the teardown removes is read before anything
   is published or deleted: the union above, so a failure to read it leaves
   everything as it was. Phase 1 **defers every Namespace that holds the release
   state of any Release in that set** (the Releases' release namespaces, which
   `sharedNamespaces` ownership by a Release or a plain destination also
   covers, whichever Release owns the Namespace object): deleting such a
   Namespace would destroy the state that the wait and phase 2 read, and the
   anchor with it. A Release this unit may not remove (one another unit owns,
   or one applied for another publication) is left with its objects, and its
   state Namespaces are protected like the rest; the phase reports each one with
   `NYL-KUBERNETES-RELEASE-LEFT-BEHIND`, a warning, and the wait does not wait
   for it. Release state is kept. Git first, like a publication, so the record never lags the cluster;
   that commit is phase 1's `published` evidence. A purge failure is a
   `Failed` outcome that a new run converges. The simpler alternative to
   deferring, a per-target marker outside those Namespaces, would need an
   object in a place Nyl does not own and a cleanup of it; deferring costs
   only the order.
2. **Remove the rest.** Once the wait ends: delete every revision of each
   Release's state, then the deferred Namespaces, each by its `Delete` option
   (so `deletePolicy: Automatic` removes it, and `Confirm` and `Retain` keep
   it as a remnant), then remove the files the index owns and the index
   (`prepare_target_removal_fenced`). Cluster state goes first so the index's
   owner keeps fencing until it is done; deleting what is already deleted is
   idempotent.

`teardownWait: observe` (the default with `observe: {}`) asks the reader which
Releases are still present in the removal sense: an object the purge would
delete (what a prune reads, as above) other than a Namespace still exists, for a
Release this unit's teardown removes. One rule decides that
(`direct::not_removable`), shared by the teardown's classification and this
reading: the latest revision was applied for this publication and is owned by
this unit or an earlier incarnation of it, or by no unit (an apply by hand of
this target took it over: a teardown removes it, the purge of a vanished Release
leaves it). A Release another publication or another unit owns counts as gone,
because the teardown leaves it. No Namespace ends or extends the wait, because phase 1 defers every
Namespace that holds the state of any Release it purges, those of Releases that
left the tree included. The wait covers the workloads.
`delay` and `manual` are unchanged. Removal is concluded only from the anchor
and a successful list. No evaluation code changes: the present set is what
`reconciler::evaluate::removal` returns as remaining.

**Readiness.** The teardown settings recorded at resolution become the
reconciler's own: the settings keep one shape, a direct target's Namespaces
with their `deletePolicy` being the part it uses, and the publication driver
evaluates them by the unit's recorded reconciler kind (`readiness.rs`); the
shell displays the findings unchanged.

| Requirement | Setting | If unmet |
| --- | --- | --- |
| Owned Namespaces are removed | namespace `deletePolicy` is `Automatic` | Remnant: empty Namespaces remain (`NYL-TEARDOWN-NAMESPACES-REMAIN`) |
| Resources with `Delete=false` or `Delete=confirm` | Not statically knowable beyond Namespaces | Not checked |
| The credentials may delete | RBAC | Not checked |

The Argo CD rows that stop a teardown (the catalog prunes, an Application
deletion policy deletes workloads, a Namespace rendering `Prune=confirm` or
`Delete=confirm` holds an Application's finalizer) have no counterpart, because nothing here holds a
finalizer: a kept Namespace is a remnant, not a blocker, so a direct target
has no blocking requirement and `NYL-TEARDOWN-CATALOG-MANUAL-SYNC`,
`NYL-TEARDOWN-APPLICATIONS-ORPHANED`, `NYL-TEARDOWN-NAMESPACE-CONFIRM`, and
`NYL-TEARDOWN-CATALOG-REMAINS` never arise. Namespace policies mean: `prunePolicy`
decides whether a Namespace a Release stops declaring is deleted by the next
apply (`Confirm`, the default, keeps it tracked with the warning above),
and `deletePolicy` whether the teardown deletes it. Unlike Argo CD's readiness,
which blocks on either `prunePolicy` or `deletePolicy` being `Confirm`, a direct
target's teardown reads `deletePolicy` alone, because the purge consults only
each object's `Delete` option and `namespacePrunePolicies` is recorded for
readiness evaluation by the recorded reconciler kind and ignored here; to remove
Namespaces with a teardown, declare both `{prunePolicy: Automatic,
deletePolicy: Automatic}`, which also keeps the same group valid for an Argo CD
target.

#### Recorded facts and keys

A unit that observes records, under `/reconciler`, the neutral part and the
location, exactly as for Argo CD:

```yaml
reconciler:
  kind: direct
  cluster: kind-dev
  context: kind-dev                 # spec.live.context; recorded, never keyed
  server: https://127.0.0.1:6443    # the Cluster's declared destination server; keyed (absent for a destination by name); a verified one proves visibility
  publication: {url: …, branch: deploy, pathPrefix: targets/dev}
  expected:
    - id: platform/web              # <group>/<release>; unique within the target
      display: web/web              # <release namespace>/<release name>
      role: workload
      path: targets/dev/platform/web
      release: platform/web
      source: {repository: …, path: targets/dev/platform/web}
      locator: {name: web, namespace: web, destinationNamespace: web}   # opaque to everything but the direct reader
  anchors:
    - {id: release-state}
```

`ReconcilerLocation::Direct(DirectLocation)` is the new variant, with its
renderer next to Argo CD's in `nyl_render::gitops::observation`, derived from
the compilation metadata (a Release's group, name, namespace, destination
namespace, and directory) instead of the catalog's Application manifests.
`unkeyed()` is `["/context"]` (`/publication/url` stays unkeyed for every
kind), so renaming a kube context never re-executes the unit; the server, the
Release identities, the paths, and the anchor key it, which the input record
already covers because the Cluster and the target are recorded inputs. The
index's `reconciler` member and the unit's `PublishedTree` record the kind.

#### Observed values

Observed values work unchanged in shape. Resolution records the owning
deployment of each observed resource (`observationSources`) and the workload
Cluster; `owning_application` becomes `owning_deployment`, which for a direct
target finds the Release directory that renders the resource, a manifest
without a namespace belonging to the Release's recorded destination namespace.
At reading time the gate "the deployment runs the current publication" is the
same restricted evaluation (`restricted_to` the owning deployment, the same
`evaluate`) over the `DirectReader`, and the field is then read from the
workload Cluster through the same context, which for a direct target is the
delivery Cluster. `pending`, `failed`, and `NYL-KUBERNETES-OBSERVATION-UNOWNED`
mean what they mean today.

#### Previews and the shared catalog

- `catalogApplication.shared` is invalid on a direct target, and a direct
  target neither needs nor joins a shared catalog. The shared catalog's
  include pattern syncs `_nyl/shared/<name>/` and `*/_nyl/catalog/*`, which a
  direct tree does not contain, so a direct target may publish to a branch that
  carries a shared catalog, without the `NYL-KUBERNETES-SHARED-CATALOG-CONFLICT`
  rule that requires every target on that branch to share it. Its prefix still
  must be disjoint from every other target's and from `_nyl/shared`.
- An EnvironmentTemplate's inline target may declare `reconciler: direct`.
  Instances of one template then fence by environment-qualified names exactly
  as Argo CD instances do, now for Release namespaces and names
  (`NYL-KUBERNETES-DIRECT-RELEASE-COLLISION`), checked over the sample
  instances. Expiry, `teardown --all`, and `state delete` run the direct
  teardown above.
- The Argo CD name uniqueness rule is not evaluated for direct targets.
- `nyl build --shared-catalog` on a direct target is refused with the
  existing `NYL-KUBERNETES-BUILD-NO-SHARED-CATALOG`.

#### End-to-end harness

A follow-up adds the `nyl-e2e` crate: one scenario, run three ways against a
Kind cluster with a local registry. It needs:

- the reference platform scenario's Kubernetes publication, which changes only
  its target between runs (the direct target has its own Cluster resource,
  with a `live.context`, pointing at the same Kind cluster, so its server is
  verifiable; Release Namespaces declare `namespace.deletePolicy: Automatic`
  so a teardown ends with an empty cluster): `reconciler: direct`, an Argo CD target read
  through the Kubernetes API (`access.method: kubernetes`), and an Argo CD
  target read through the API server (`access.method: api`);
- Kind with the local-registry recipe, so an image pushed by `OciImage` as
  `localhost:5001/…` is pulled by the nodes under the same reference;
- for the Argo CD runs, an Argo CD installed from a pinned manifest, a Git
  endpoint the Argo CD pod can reach for the publication repository (the
  repository URL Argo CD syncs is distinct from the push URL Nyl uses), the
  catalog Application applied once, and, for the API method, `server.insecure`
  so a loopback `http://127.0.0.1:<port>` address is valid, a local account
  `nyl` with a policy granting `get` on the instance's Applications, and its
  token admitted as `ARGOCD_AUTH_TOKEN`;
- the unit's `env.passthrough: [KUBECONFIG]`, for the context to the Kind
  cluster, which the Cluster declares as `spec.live.context`, for all three;
- the same assertions in each run: the same Release directories in the
  published tree and equivalent receipts, `accepted` and `healthy` after
  polling `nyl attest` with a timeout, the same observed value, and teardown
  to an empty cluster with `teardownWait: observe`.

Tier-1 scenarios cover the engine rules with test drivers and the fake observer
as before, plus a `FakePublication` mode that mimics a direct target's
attestation and wait; real direct apply is tested in `nyl-units` against
`MockKubeClient` with an in-memory `ReleaseStorage`, and for real in the
harness.

#### Code homes and the driver boundary

| Concern | Owner |
| --- | --- |
| `reconciler` field, validation, `Delivery`, tree without Applications, index `reconciler`, direct renderer of expected deployments, `without_workloads`, `TeardownSettings` for direct | `nyl-render` (`resources/gitops.rs`, `gitops/`) |
| Apply, prune, purge, release state with `meta`, `verify_cluster_server`, `release_manifests` | `nyl-render` (`kubernetes/lifecycle.rs`, `kubernetes/state.rs`) |
| Direct driver operations (preflight, waves, vanished Releases, fence, outcome mapping), `DirectReader`, health rules, readiness rows | `nyl-units` (`kubernetes_publication/direct/`) |
| The wait for health (`observe.waitHealthy`): the poll loop, backoff, early failure, and codes, over any reader | `nyl-units` (`kubernetes_publication/health_wait.rs`), with `ExecutionContext::clock` from `nyl-core` |
| Wiring, `--take-over`, `ApplyEvent` rendering | `nyl` (the CLI role) |
| Engine, state, `Driver`, `RemovalObservation`, `RecoveryPolicy`, `Outcome` | Unchanged: they never name `direct`, a release, or a kube object |

The engine, the state formats, and the CLI workflows learn nothing: the
driver already reports a deployment set, a reading, a removal, an owner
fence, and an outcome through generic contracts, and the unit's `observe`
stays kind-agnostic. The reader, health rules, fence variants, and codes
(`NYL-KUBERNETES-RELEASE-FENCED`, `-RELEASE-OWNER-UNKNOWN`, `-RECONCILER-CHANGED`,
`-CLUSTER-CONTEXT-MISSING`, `-DIRECT-RELEASE-COLLISION`,
`-PRUNE-AWAITING-CONFIRMATION`) are the publication driver's. The only
additions outside the driver are declaration and rendering of the target
(`nyl-render` owns Kubernetes) and `ReleaseState` metadata.

Follow-ups, not part of the first version:

| Item | Needed by |
| --- | --- |
| A `Dedicated` shared-namespace owner for a direct target: a synthetic Release `<group>/_ns-<name>` to hold its release state (rejected for now) | Before direct targets are recommended for shared namespaces |
| Cluster drift detection for a direct unit | After the first direct runs |
| Skipping the write of a revision when nothing changed (the apply itself always runs, because server-side apply is idempotent and self-heals) | After revision churn is measured |
| A `nyl release delete --keep-resources` that keeps the resources and the state (`--no-purge` keeps the resources and deletes the state) | With the Argo CD to direct procedure |

## Environment templates and previews

A preview environment is an environment whose definition lives in state
instead of source. Source holds only an EnvironmentTemplate describing what
such an environment looks like; each instance is created from it with
parameters.

```yaml
apiVersion: gitops.nyl/v1
kind: EnvironmentTemplate
metadata:
  name: preview
spec:
  parameters:
    pr: {type: integer}
  unitSelector:
    matchLabels: {preview: 'true'}
  values:
    stateKey: 'previews/pr-{{ params.pr }}'
    hostname: 'pr-{{ params.pr }}.preview.example.com'
  state:
    repositoryRef: {name: platform-state}
    desiredRef: nyl/previews
    observedRef: nyl/previews
    path: '{{ environment.name }}'       # all instances share one ref
  allowUnprotectedSource: true           # instances run from pull request branches
  # keepSource: true                     # opt in: keep refs for instances that outlive their branches
  deletionPolicy: Teardown               # forced for every unit kind that supports teardown
  allowTeardown: true                    # omission teardown without --allow-teardown
  ttl: 7d
  maxInstances: 20
```

The template has the Environment's fields plus `parameters` (a map of names to
Release input declarations, `type` with optional `default`, `enum`, and
`description`, exposed as `params`; `--param <name>=<value>` parses the value
as YAML and checks it against the type), `deletionPolicy`, `allowTeardown`, `ttl`,
`maxInstances`, and `keepSource`. The template's `deletionPolicy: Teardown`
overrides every unit's own policy, including an explicit `Retain`, for kinds
that support teardown. Units of other kinds, such as `OciImage`, are left in
place and dropped from state when an instance is removed, and the transition
commit lists what was left, such as the image reference. Removing unused
images is the registry's retention policy's job. The template knows nothing
about Kubernetes; a preview's cluster and publication come from
`KubernetesPublication` units with inline targets.

### Trust and credentials

An instance renders its template and its units at its own source commit, so a
pull request controls everything about its preview, including the template.
Nyl does not enforce a trust boundary between pull requests and the rest of the
project; credentials do, and that is a property of the pipeline:

- Preview jobs receive credentials scoped to preview resources: push access to
  the preview state and deploy refs, a registry path, and a cloud role limited
  to the preview backend prefix. The scheduled fleet reconcile runs with the
  same preview credentials, never broader ones.
- Pull requests from forks run preview jobs only after a maintainer approves
  the workflow run, as CI systems already provide.
- Teams that want template changes reviewed before they take effect protect
  the template files with code-owner review; Nyl still reads the template at
  the instance's source commit.
- A template that sets `keepSource: true` makes the source repository take
  the instances' keep refs (`refs/nyl-keep/*`, see [Instances](#instances)),
  so its preview jobs need push access to that namespace. By default nothing
  is pushed to the source repository.
- State may live in one ref for every environment through `state.path`, in
  one ref per template, or in one ref per instance. Branch protection follows
  the layout: the more state a ref holds, the more a preview credential can
  reach.
- Secrets never pass between units as values. A unit that creates a secret,
  such as a database password, stores it in a secret store and outputs only
  its name or reference; the consuming Release reads it from there, for
  example through an ExternalSecret. `sensitive` outputs exist so a tool's
  secret output can be validated without ever being recorded.

### Instances

Instances use the ordinary commands:

```bash
nyl reconcile -e pr-123 --template preview --param pr=123    # create or update, extend, and reconcile
nyl state init -e pr-123 --template preview --param pr=123   # create or update without reconciling
nyl get units -e pr-123 / nyl status -e pr-123
nyl teardown -e pr-123 --all                                 # tear down every unit, honouring Retain
nyl state delete -e pr-123                                   # remove the instance's state, leaving the deleted marker
nyl get environments                                         # declared environments and instances
```

- `nyl state init --template` writes the instance's `state.yaml` with the
  template name, parameters, source commit, and expiry. Run again for an existing instance, it
  updates the parameters; the next reconcile applies them. It refuses a name
  already used by a declared Environment or by an instance of another
  template. Afterwards the instance behaves like a declared environment for
  every command.
- `state init --template` and `reconcile --template` render the template at
  the entry worktree and record the bound parameters; `--param` overrides
  individual recorded values when an instance is updated, and a parameter
  without a `default` is required. Every other command finds an instance by
  its record, never from ambient input: it reads each template's state
  location for the name (`state` renders with `environment.name` alone,
  because it locates the `state.yaml` that holds the parameters), and the
  record that names the environment and the template is the instance. A name
  with no record is unknown (`NYL-ENVIRONMENT-UNKNOWN`); records of two
  templates are `NYL-INSTANCE-AMBIGUOUS`.
- Instances are discovered by listing the template's state location, so no
  central index exists. Listing needs one location for every instance: a
  `state.path` that ends in `{{ environment.name }}` and refs and a repository
  that do not read the name. A template whose refs depend on the name has its
  instances found by name only, and `get environments` and the fleet job do not
  list them. Changing a template's `state` hides the instances at the old
  location from the fleet job, the close jobs, and every command that names
  one (`NYL-ENVIRONMENT-UNKNOWN`), because the old location is recorded
  nowhere in source, and they never expire. Change the `state` and run
  `nyl state move --template` in the same change, before the scheduled job
  runs from the new definition; `nyl state move --template <name> [--state-path …
  --desired-ref … --observed-ref … --coordination-refs … --from …]`, with the
  old location's options like `state move`, moves every instance found by
  listing the old location, one `state move` each. The old `--state-path`
  ends in `{{ environment.name }}` like the template's, and the old location
  must be listable the same way. Without any old-location option it refuses
  (`NYL-STATE-LOCATION-MISMATCH`); naming no instance at the old location
  refuses too (`NYL-STATE-MISSING`), and moving again finds the instances
  already there. A template renamed in source leaves its instances' records
  naming the old one, so no command finds them (`NYL-INSTANCE-TEMPLATE-UNKNOWN`):
  `nyl state move --template <new> --from-template <old>` records the
  instances found at the new template's location as the new template's, one
  transition commit each, and combined with old-location options moves them
  first. An instance renders at its recorded source commit, which predates the
  rename, so its next run is its pull request job's `reconcile --template
  <new>`, which moves the recorded commit; closing it needs no source.
- `state init --template` records its entry worktree's commit as the
  instance's source commit (`sourceCommit` in `state.yaml`), so a later plain
  run of the instance has one: it renders at that commit and records
  `sourceChosenBy: {by: recorded}`, unless the template's `source` names a
  revision or `--source` overrides it for the run.
- `nyl get environments` lists the declared environments and then the live
  instances found by listing, with their template and expiry; an instance
  whose state was deleted is not listed.
- Every entry point sees instances through one path. `nyl validate` cannot
  read state, so it renders sample instances of each template (distinct names
  and parameters, taken from `enum` values or generated by type), and the
  checks across environments cover them: state locations, publication
  prefixes, Argo CD Application and AppProject names, and ownership scopes.
  Every other command adds the instances state lists to the same checks,
  including each publication render and the placements a direct `build
  --push` consults, so a push into an instance's target is refused.
- `nyl teardown -e <env> --all` tears down every unit whose deletion policy
  allows it, dependents first, and then holds every unit that is still in the
  recorded ownership set, so nothing is recreated. It honours
  `deletionPolicy: Retain`: units that declare `Retain`, retained tombstones,
  and units whose kind has no teardown support are released instead. Their
  resources are left in place, they are listed under `details.released` in
  the transition commit, and a released unit inside the ownership set becomes
  a held desired file without an incarnation, so a later `reconcile` does not
  rebuild an image or re-run a command; one outside it is dropped from state.
  Consumers of a publication are torn down before it, and the units the
  publication depends on wait until its teardown completes under its
  `teardownWait`, so with the `manual` strategy `teardown --all` stops after
  the publication and exits 2 until `--confirm-removed`. A decommission that
  does not finish in one invocation leaves its remaining units `deleting`
  with `reason: decommission`; `--confirm-removed`, running `teardown --all`
  again, or a plain `reconcile` continue it, and the run that completes a
  unit's teardown applies the hold the request recorded
  (`nyl_orchestration::shell::run::teardown`).
- `nyl state delete -e <env>` removes an environment's state in one
  transition commit (owner: `nyl_orchestration::shell::teardown`). It works
  for template instances and declared environments alike, once no unit has an
  incarnation left (`NYL-STATE-DELETE-INCARNATIONS` otherwise, exit 1, which
  also refuses while run refs wait for an import, naming `nyl reconcile` and
  `nyl recover`). The commit removes every file of the environment's state
  on both refs, including held desired files and records of units that were
  left in place, and leaves only `state.yaml`, whose last event is
  `state-deleted` with who deleted it, when, and why: the deleted marker. The
  refs and their history stay, and so do other environments' files on a shared
  ref; the environment's evidence and signal refs are deleted.
  `--teardown` runs `teardown --all` first and continues to the deletion in
  the same command exactly when, re-reading state after the teardown, no
  desired document has a uid and no unit is deleting or holds a pending
  teardown request. Otherwise it stops: exit 2 when every remaining unit
  waits (a teardown wait, an approval, dependents, readiness, or a held
  unit), and exit 1, with the failed or uncertain units in the report, when
  a teardown failed or is uncertain. Running it again resumes. A broken
  lease refuses both forms with `NYL-LEASE-BROKEN` until `nyl recover -e
  <env>` imports the dead run.
- Decommissioning a declared environment is `nyl state delete -e <env>
  --teardown`, then removing its Environment from source. Until the
  Environment is removed, every command for it, a plain `nyl state init`
  included, fails with `NYL-STATE-DELETED`, exit 1, because its `state.yaml`
  carries the deleted marker; the message names both ways forward (removing
  the Environment, or `state init --fresh`, which appends a
  `state-initialized` event and starts over), so a pipeline that still runs
  it fails visibly instead of silently re-creating anything. Detection reads
  the marker in `state.yaml` (`rules::state_location::check_record`), never
  commit trailers or history. The recommended layout for previews is a
  shared ref with `state.path`, so an instance's state is a directory;
  separate refs per instance need permission to create refs matching the
  template's ref pattern. A template instance is the exception for its pull
  request job: `reconcile -e <instance> --template <name>` treats an instance
  whose state was deleted as missing and creates it again, as a new instance
  under `maxInstances`, so a pull request whose preview the fleet job removed
  gets it back with its next push; `state init --template` still needs
  `--fresh`. Whether the record is deleted is decided under the lease, on the
  record the write reads: of two jobs that saw it deleted, the first restarts
  the instance and the second finds it live and only updates it, so the second
  never drops the first's documents.
- By default Nyl pushes nothing to the source repository: an instance is torn
  down by the pull request closed job, which runs `state delete -e <instance>
  --teardown` while the recorded commit is still reachable. With `keepSource:
  true` (an opt-in, for previews that may expire, through the fleet, long after
  their branches are gone), Nyl keeps each instance's recorded source commit
  fetchable with a keep ref in the source repository, so teardown after the
  pull request branch is deleted or squash-merged still finds its source. The
  refs are `refs/nyl-keep/<keepId>/<commit>`, one per commit and never moved:
  - `<keepId>` is minted from the injected id generator when an instance first
    keeps a commit and recorded in its `state.yaml` (`keepId`); every lookup and
    release reads it from the record. It is unique across projects of one
    source repository, and survives moves of the state and of the project
    directory and renames of the template, because nothing about the name is
    derived from where the instance lives. A restarted instance (a deleted
    one that starts over) mints a new one. The namespace does not depend on
    `coordinationRefPrefix`, and it is reserved: a state ref or prefix inside it
    is `NYL-STATE-REF-CONFLICT`.
  - `state init --template` and `reconcile --template` create the ref of the
    commit they record under the lease, past every refusal and before the
    record is written, and remove the instance's other refs after the record is
    committed. The recorded commit is therefore never unkept, a refused write
    pushes nothing, and a ref that already names another commit is an error,
    never overwritten.
  - A push the source repository refuses refuses the job
    (`NYL-INSTANCE-KEEP-FAILED`, exit 1) before any state is written, rather
    than promise a teardown it cannot deliver. The keep refs need push access
    to `refs/nyl-keep/*`, which a read-only token does not have, such as
    GitHub's default `contents: read` or a fork pull request's; the finding
    names both ways out: grant the access, or leave `keepSource` off. A
    run with `--local` pushes nothing.
  - Every command that needs a recorded commit the clone lacks fetches it from
    its ref. `state delete` removes the instance's refs after its deleted
    marker is committed, and leaves a ref that does not name the commit its
    name says (not this instance's) with a warning; a ref that cannot be
    removed is a warning too, because it is no reason to keep the state.
- The recommended teardown is the pull request closed job: it checks out the
  default branch and runs `state delete -e <instance> --teardown`, with no
  `--source`. The recorded commit is the pull request's last head, which the
  host keeps reachable through its pull request ref after the branch is
  deleted or squash-merged, and Nyl fetches it by id from the clone's `origin`
  remote (see [Source](#source)); no keep ref is needed. The fleet job fetches
  the same way, so an expired preview whose branch is gone is removed without
  keep refs on such a host. The preview job must therefore record the pull
  request's head: it checks out `${{ github.event.pull_request.head.sha }}`
  on GitHub, because the default checkout of a `pull_request` event is the
  synthetic merge commit `refs/pull/<n>/merge`, which the host does not keep
  once the pull request closes. A host without pull request refs, history the
  host may prune before an instance expires, and a job run with `--local` need
  `keepSource: true` or `--source`.
- When an instance's recorded source commit cannot be fetched, teardown fails
  with a message suggesting `--source <revision>`. `nyl teardown --all` and
  `nyl state delete --teardown` accept `--source` to take the units' source
  files from another revision, typically the protected branch right after the
  pull request was squash-merged. Teardown uses the stored resolved specs, so
  only the source files come from that revision, including those of a source
  field the stored spec pinned to the recorded commit without an explicit
  `revision` or `commit` lock; explicit locks and other repositories stay as
  declared. When they differ from the recorded source, Nyl warns and records
  the substitution in the transition commit; it never refuses, so a lost
  commit cannot lock anyone out of removing an instance.
- An instance's units read shared infrastructure from declared environments
  with a cross-environment reference, such as
  `fromUnit: {environment: dev, unit: network, output: vpcId}`, under the
  rules of [Cross-environment references](#cross-environment-references),
  including the protection of the units they read.
- Instances cannot be a PromotionPath source; previews build and test, and
  promotion starts from declared environments.

As implemented: a template's `values`, `source`, and
`protectedRefs` strings render with `params` and `environment.name` when an
instance is loaded, so a `values.target` object carries concrete prefixes and
names into a unit's `target: {{ values.target | tojson }}`. `deletionPolicy`,
`allowTeardown`, `ttl`, `maxInstances`, `keepSource` with its keep refs,
sliding expiry and the removal of expired instances, `--renew`, `--no-extend`,
`--make-room`, the fleet `reconcile --template` without `-e`, and `nyl state
move --template` are implemented, and so are `catalogApplication.shared`
(its manifest, its `catalog:<name>` owner, and `nyl build -- --shared-catalog`)
and cross-environment references from instances to declared environments, with
the protection of their producers; the preview reference scenarios
(`reference/preview-closed` and `reference/preview`) run them in both tiers.

### Template changes

An instance renders its template and units at its own source commit S: the
pull request job's worktree, recorded in state, so a preview always shows what
that pull request would deploy, whichever job reconciles it.

- A template change on the default branch reaches an instance when the pull
  request picks it up and CI reconciles the new head. Changed values, units,
  or inline targets change execution keys, so exactly the affected units run
  again; a unit removed from the template is torn down, without
  `--allow-teardown` when the template sets `allowTeardown`.
- `nyl reconcile --template preview` reconciles every instance, each at its own
  recorded source commit: after a Nyl upgrade that changes rendered output, a
  driver behavior-version change, or new evidence in shared infrastructure. It is maintenance, not
  activity: it never extends expiry, and it removes expired instances (see
  [Expiry](#expiry)). An instance whose lease is held is skipped and reported
  as `busy`, not as a failure, because another run is reconciling it; the
  exit category reflects only the instances the fleet run processed, and a
  busy expired instance is removed by a later run. The fleet finds the
  instances by listing the template's state location, so it needs a listable
  one (`NYL-INSTANCE-NOT-LISTABLE`); an unreadable listing fails the run
  (`NYL-STATE-UNAVAILABLE`) instead of sweeping what it could not see. It
  names no source: `--source` and `--worktree` are rejected, because each
  instance runs at its own recorded commit. The removal of an expired instance
  does not render its source, so a recorded commit that is gone fails only the
  teardown that needs its files (`--source <revision>` supplies them) and
  never the sweep of the other instances. It never renders an
  instance's units with a template from another commit, because that
  combination was never reviewed together; pulling the default branch into
  every pull request remains a pipeline choice.

### Expiry

Nyl has no daemon, so expiry works through ordinary CI runs, and a continuous
runner (M7) could add timely enforcement later. Whether an instance is expired
is decided from its record's `expiresAt`, the template's `ttl`, and the
injected clock (`nyl_orchestration::rules::expiry`); a template without `ttl`
never expires its instances, whatever an older record says. An instance is
expired from the moment `expiresAt` is reached.

- `state.yaml` records `expiresAt`. Only activity on one instance extends it:
  `state init --template`, `reconcile -e <instance> --template <name>`, and
  every other `reconcile -e <instance>` that finds the instance live and is not
  `--no-extend` move it to now plus `ttl` when they start, in a commit of its
  own (operation `state-renew`, none when the time does not change), whether or
  not the run succeeds, so an active pull request keeps its preview even
  while its preview fails. `plan`, `status`, `get`, and `teardown` never
  extend it.
- Expiry is the declared intent to clean up, so every reconcile that finds an
  instance expired removes it instead of reconciling it, checking expiry
  before it extends anything: `reconcile -e <instance>`, `reconcile -e
  <instance> --no-extend`, and `reconcile --template <name>`. Removal runs
  `teardown --all` and then `state delete`, with the teardown's units
  recorded as deleting with the reason `expired`. Nothing is deleted while a
  unit has an incarnation, a teardown in progress, or a recorded request, so
  the state stays until every teardown completed, and the run's report says
  what remains (`action: removal-incomplete`; exit 2 for waits, 3 and 4 for a
  failed or uncertain teardown). The teardown starts only when the template's
  `allowTeardown` (read at the entry checkout, which is the scheduled job's
  trusted one) or `--allow-teardown` authorizes it; without either, the units
  are recorded as pending teardown, the instance is reported as pending
  (`action: removal-pending`, `NYL-INSTANCE-REMOVAL-PENDING`), and the run exits
  2. The report says pending only when units were held back for that reason,
  never for a lease another run holds or an instance renewed meanwhile. An instance whose teardown readiness is incomplete is reported and left in
  place unless the run passes `--allow-incomplete`, and the options of `reconcile`
  that gate a teardown (`--allow-dependents`, `--approve`, `--source`) apply to
  it, except that an eviction by `--make-room` inherits neither `--source` nor
  `--approve`, which name the new pull request's instance. Removal also waits for each publication's `teardownWait`: with the
  `manual` default of a unit without `observe`, it stops after phase 1 until
  `--confirm-removed`, so templates meant to expire unattended use `observe`
  or `delay`. A removal stuck in phase 1 needs `--confirm-removed` (or
  `teardown --all` run again once the wait is over); until then the preview
  is half torn down, every reconcile of it, the pull request job's included,
  exits 2, and nothing recreates its workloads. `state init --template` warns otherwise
  (`NYL-TEARDOWN-WAIT-MANUAL`, read from the unit's declaration, so it shows
  before any producer ran; the unmet readiness requirements of the unit are
  shown for the units that can resolve then, and by `plan` for the rest). The decision is made
  from the record the command read and repeated under the lease: a record
  whose `expiresAt` changed since (another run renewed or removed it) leaves
  everything in place (`NYL-INSTANCE-RENEWED`, exit 2). `status`, `plan`, and
  `get environments` show expired instances until they are removed
  (`NYL-INSTANCE-EXPIRED` warning).
- `--renew` is the only way to keep an expired instance by a plain
  reconcile: it extends the expiry and then reconciles.
- A renewal, whichever command renews, undoes a removal that was recorded and
  has not started: in the commit that moves the expiry, under the lease, the
  units that `teardown --all` marked as deleting for the reason `expired`,
  pending, are active again (lifecycle event `renewed`; the table does not
  return `expired` deletions on any other event). A teardown that has started
  continues, because effects exist that only the teardown ends: the next
  reconcile completes it, and the units are created again after it completes.
  Without this, a preview renewed after an unauthorized removal would stay
  fenced, and a later run with `--allow-teardown` would tear a live preview
  down.
- `reconcile -e <instance>` extends a live instance's expiry;
  `reconcile -e <instance> --no-extend` reconciles it as maintenance without
  extending; `reconcile --template <name>` never extends.
- `--renew` and `--no-extend` conflict and are rejected together. Both are
  rejected for declared environments, which have no expiry, and for the fleet
  reconcile, `--template` without `-e` (`NYL-INSTANCE-OPTION-INVALID`, exit 1),
  as are `--param` and `--make-room` there. `--no-extend` is also rejected with
  `--template`, which is activity by definition; `--renew` with it is
  redundant and accepted. Each of these is the coded finding, not a usage error,
  as are `--renew` and `--no-extend` with a label or group selection.
- The pull request job's `reconcile -e <instance> --template <name> --param …`
  is activity by definition, so it renews an expired instance instead of
  removing it: updating an active pull request never removes its preview.
- A pull request pipeline runs `reconcile -e <instance> --template <name>
  --param …` on every push. It creates the instance when it is missing,
  updates its parameters when they changed, extends the expiry, and
  reconciles, so updating an active pull request never removes its preview. `state delete --teardown` removes one instance explicitly,
  expired or not.
- `maxInstances` counts the template's live instances, expired ones included
  until they are removed, and limits creation only: updating an existing
  instance is never refused, and a deleted one starts over (`--fresh`) as a
  new one. It needs the instances to be listable, so `nyl validate` rejects a
  template with `maxInstances` whose state location depends on the instance
  name, and creation refuses (`NYL-INSTANCE-NOT-LISTABLE`). At `maxInstances`,
  `state init --template` and the pull request job's `reconcile --template`
  refuse with exit 2 (`NYL-INSTANCE-LIMIT`) and write nothing, naming the
  expired instances that could be removed. Removing them is the scheduled
  fleet job's work, never a side effect of another pull request's job:
  hitting the limit is a project configuration concern, not the new pull
  request's. `--make-room` opts in to removing expired instances first, the
  same way the fleet job does (the same removal, authorized by
  `allowTeardown` or `--allow-teardown`, with `--allow-incomplete` for
  incomplete readiness), and then creating the instance. It removes the
  expired instances with the oldest `expiresAt` first and only as many as the
  new instance needs, never a live instance. When too few instances are
  expired, or a removal does not finish, nothing is created and the refusal
  says which (`NYL-INSTANCE-LIMIT`, exit 2). An expired instance renewed in
  the meantime is skipped, and a removal that does not finish is left, and
  `--make-room` goes on to the next expired instance until enough are removed.
  The removal of another pull request's preview is authorized by
  `allowTeardown` of the template as this job's checkout declares it, the
  trust point of the preview credentials: Nyl enforces no boundary between pull
  requests (see [Trust and credentials](#trust-and-credentials)), and a job
  that cannot be trusted with that cannot hold credentials that reach other
  previews. Creation under a limit is not
  atomic across jobs: two jobs creating at once can exceed it by one, which
  the next `--make-room` or fleet run absorbs.

A typical pipeline runs `reconcile -e <instance> --template <name> --param …`
when a pull request opens or updates, `state delete --teardown` when it closes, and
`reconcile --template` on a schedule, which both maintains live instances and
removes expired ones.

## Artifacts

Kinds with fixed results publish artifacts instead of outputs. Artifacts are
their own kinds in `artifacts.gitops.nyl/v1`:

| Kind | Published by, as | `spec` |
| --- | --- | --- |
| `ContainerImage` | `OciImage`, named `image` | `repository` (string), `digest` (`sha256:<hex>`), `reference` (`<repository>@<digest>`), `platforms` (list of strings), `tags` (list of strings) |
| `PublishedTree` | `KubernetesPublication`, named `tree` | `repository` (publication URL), `branch` (string), `pathPrefix` (string), `target` (the DeploymentTarget's name), `commit` (commit ID), `indexDigest` (`sha256:<hex>` over the canonical JSON of `<prefix>/_nyl/index.json`), and, for a target with `catalogApplication.shared`, `sharedCatalog` (`pathPrefix` of the catalog's tree and `result`: `created`, `current`, or `differs`) |

Each kind publishes its artifacts under fixed names, so references name them
without configuration. An artifact document has `apiVersion`, `kind`,
`metadata.name`, `metadata.unit` (`{environment, name, uid}`), the
`executionKey` of the receipt that published it, and `spec`.

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
  tags: [nyl-dev-1a2b3c4d5e6f7a8b, dev]
---
# observed/units/kubernetes/artifacts/tree.yaml
apiVersion: artifacts.gitops.nyl/v1
kind: PublishedTree
metadata:
  name: tree
  unit: {environment: dev, name: kubernetes, uid: 91c3…}
executionKey: sha256:…
spec:
  repository: git@github.com:acme/deploy.git
  branch: deploy
  pathPrefix: dev
  commit: 9c1e…
  indexDigest: sha256:77d0…
```

A direct build (see [Direct builds](#direct-builds)) prints the artifact it
produced, in the same document shape. A kind may also print documents of
other kinds that only a build prints and nothing records: `KubernetesPublication`
prints a `RenderedTree` for a plain build and a `TreeDiff` for `--diff` (see
the [publication unit](#kubernetes-publication-unit)). Their `metadata.name`
is `tree` and `diff`, no receipt lists them, and no reference reads them.

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
admission.yaml                               # current DeploymentAdmission (M8), on the admission ref only
desired/
  environment.yaml                           # resolved Environment, source commit, ownership set
  units/<unit>.yaml                          # DesiredUnit, including lifecycle (deletion, hold)
  promotions/<path>.yaml                     # PromotionRecords
observed/
  units/<unit>.yaml                          # ObservedUnit: latest receipt and condition
  units/<unit>/artifacts/<name>.yaml         # artifacts of the latest receipt
  units/<unit>/drift.yaml                    # latest drift detection of the current receipt
  units/<unit>/observations.yaml             # latest observed values of the current receipt
  units/<unit>/attestations.yaml             # latest attestation per name for the current receipt
  attestations.yaml                          # environment-wide attestations for the current state
```

With separate refs, `desired/` exists only on the desired ref and `observed/`
only on the observed ref; `state.yaml` exists on both. `admission.yaml` exists
only on the admission ref, which is the desired ref unless `state.admissionRef`
names another.

Every state file is YAML. State records use `apiVersion: gitops.nyl/v1` with
the kinds `StateRecord`, `EnvironmentRecord`, `DesiredUnit`, `PromotionRecord`,
`ObservedUnit`, `DriftRecord`, `ObservationRecord`, `AttestationRecord`, `Lease`, `Run`, and `Signal`, plus
`DeploymentAdmission` from M8; artifacts use their own
kinds. JSON Schemas for all of them are generated from the Rust types and
published with the other resource references. Digests, including execution
keys, are computed over a canonical JSON form (RFC 8785), so formatting never
affects them. An integer beyond ±(2^53 - 1), which RFC 8785 would round to a
double, is written with its exact decimal digits, so two values never share a
digest. The version in `apiVersion` is the format version of each kind:
a format change adds a version with a migration from the previous one, golden
files cover every version, and a reader rejects a version it does not know.

| Kind | Location | Content |
| --- | --- | --- |
| `StateRecord` | `state.yaml` | `environment`, `template` and `params` for instances, `expiresAt`, `sourceCommit`, `keepId`, `location` (repository, refs, `path`, `coordinationRefPrefix`), and `events`: `state-initialized` (with `fresh`), `state-moved`, `state-restored`, `state-deleted`, each with time, requester, and reason. A last event `state-deleted` is the deleted marker `nyl state delete` leaves (see [Instances](#instances)) |
| `EnvironmentRecord` | `desired/environment.yaml` | The resolved Environment spec with group defaults applied, `sourceCommit` S and how it was chosen (`entry`, `revision`, `commit`, `promotion` with the record's path and `sequence`, `admission` with the generation, or `override`), `appliedPromotionSequence`, and the ownership set as unit names with uids |
| `DeploymentAdmission` (M8) | `admission.yaml` on the admission ref | The current generation: ID, sequence, source commit and how it was chosen, semantic graph digest, and normalized authorizing actor, request, and reason; only a newer admission replaces it (see [Deployment admission](ci-execution.md#deployment-admission)) |
| `DesiredUnit` | `desired/units/<unit>.yaml` | See [Desired unit](#desired-unit); a `retained` tombstone is a DesiredUnit with `lifecycle.state: retained` that also embeds the last receipt, and has no observed file; a held marker is a DesiredUnit with `lifecycle.state: held` and only the unit's environment and name (see [Deletion](#deletion)) |
| `PromotionRecord` | `desired/promotions/<path>.yaml` | See the [promotion contract](promotion.md#promotionrecord) |
| `ObservedUnit` | `observed/units/<unit>.yaml` | See [Observed unit](#observed-unit) |
| `DriftRecord` | `observed/units/<unit>/drift.yaml` | `unit`, `executionKey`, `at`, `result` (`clean`, `drifted` with a summary, or `unknown` with a reason), driver-specific `details` (see [Drift](#drift)) |
| `ObservationRecord` | `observed/units/<unit>/observations.yaml` | `unit`, `executionKey`, `run` (that produced the receipt), `at` (the last read), and `values`: each declared name with `value` and `observedAt` (the last successful read), or `pending` with the driver's reason (see [Observed values](#observed-values)) |
| `AttestationRecord` | `observed/units/<unit>/attestations.yaml`, `observed/attestations.yaml` | The latest attestation per name of one scope; see [Attestations](#attestations) |
| `Lease` | the lease ref | `run`, `runner`, `operation`, `units` executing, `deadline`, `takenAt` |
| `Run` | the run ref's first commit, updated per phase; kept on an evidence ref after a takeover retires the run ref | `run`, `runner`, `operation`, `sourceCommit`, `readDesired`, `readObserved`, `deadline`, `units` executing, and `finished`; `created/<unit>.yaml`, the desired document and new uid of every unit the run created, recorded when the unit starts; and one checkpoint commit per finished unit holding its desired document, uid, and receipt or condition |
| `Signal` | the signal ref | `run`, `unit`, `action` (`confirm-removed` or `hold`), `phase1Commit` (for `confirm-removed`), `by`, `identity`, `reason`, `at`, `from` |

Each unit has one desired file and at most one observed file, holding only the
latest state. Earlier states exist in Git history, which is enough for audit
and promotion: a consumer cites a producer's receipt by execution key and the
observed commit that holds it, and promotion walks the history of one file.
A unit's drift record and attestations are one file each too, keyed by name
where several exist; `git blame` shows when each entry last changed.

### Desired unit

```yaml
apiVersion: gitops.nyl/v1
kind: DesiredUnit
unit: {environment: dev, name: database, uid: 5d2a…, apiVersion: units.gitops.nyl/v1, kind: OpenTofu}
lifecycle:
  state: active                            # active | deleting | held | retained
sourceCommit: 3e7b…
sourceRevision: 3e7b…                      # effective revision for repository content
spec:                                      # rendered, references still as expressions
  source: {path: infra/database}
  variables:
    vpc_id: {fromUnit: {unit: network, output: vpcId}}
  outputs: {host: {type: string}, port: {type: integer}}
  approval: {mode: manual, bind: plan}
resolvedSpec:                              # references replaced by values
  source: {path: infra/database, commit: 3e7b…}
  variables: {vpc_id: vpc-0abc123}
  outputs: {host: {type: string}, port: {type: integer}}
  approval: {mode: manual, bind: plan}
provenance:
  /variables/vpc_id: {unit: network, uid: 8c1f…, executionKey: sha256:…, observedCommit: 41f0…}
  # a value produced earlier in the same run cites the run instead: {…, run: 0b8f…}
executionKey: sha256:…
readiness: {state: ready}
# while blocked:
# readiness:
#   state: blocked
#   reasons: [{pointer: /variables/vpc_id, reason: network has no current receipt}]
```

- `sourceRevision` is S. Resolution writes the resolved commit into every Git
  source field of the unit, such as `resolvedSpec.source.commit` or
  `resolvedSpec.contexts.shared.commit`, whether it is S, a locked commit, a
  resolved `revision`, or a promoted revision, so a PromotionPath selector on
  `/source` carries the path together with the exact commit the source
  environment executed.
- `resolvedSpec` is what PromotionPath selectors such as `input: /source` or
  `input: /releases/platform~1web/image` point into.
- `provenance` records, per reference, the receipt or PromotionRecord the value
  came from, citing the observed commit it was read from. A value from a
  receipt produced earlier in the same run cites the run ID instead, which
  identifies the transition commit through its `Nyl-Run-Id` trailer; run refs
  are disposable, so provenance never points at a checkpoint. Provenance never
  names the commit it is written in. A citation is stable: when a reference
  resolves to the same receipt (unit, uid, execution key) that the stored
  document already cites at that pointer, the stored citation, run or observed
  commit, is kept. Re-resolving against unchanged receipts therefore changes
  nothing, and a run that executes nothing writes no commit.
- `executionKey` is a digest of the unit's `apiVersion`, `kind`, and driver
  behavior version, `resolvedSpec` without the common fields that do not affect
  what runs (`enabled`, `dependsOn`, `approval`, `deletionPolicy`, `timeout`,
  `observationWait`), and with `observations`, since a changed declaration
  reads other values for the receipt,
  the selected source bytes, and any command-unit fingerprint output. Kinds may
  declare further fields that change how a unit runs but not its result, such
  as `OciImage`'s `builder` and `cache`, and exclude them too. The key excludes
  the source commit ID (including `resolvedSpec.source.commit`; the selected
  bytes stand for it), provenance, readiness, lifecycle, Nyl's
  release version, and external tool versions, so a new source commit with
  identical inputs, a producer re-execution that yields identical outputs, or a
  Nyl upgrade does not change it.
- A blocked unit keeps its rendered spec and source commit, so a later wave of
  the same run resolves it from new evidence without rereading source. Each
  run renders every unit from its own source commit S, so the next run
  replaces a blocked unit's kept spec with that run's rendering.
- `lifecycle` carries deletion and holds (see [Deletion](#deletion)).
- A held marker, written by `nyl hold` for a unit that has no desired
  document in the environment, has `unit: {environment, name}` without uid,
  `apiVersion`, or `kind`, no source commit, no spec, and `lifecycle.state:
  held`. It is the only desired document without a rendering; every rendered
  document has all fields.

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
  readDesired: 77aa…                 # desired commit the run started from
  startedAt: 2026-09-24T10:00:00Z
  finishedAt: 2026-09-24T10:07:12Z
  versions: {nyl: 0.7.0, tofu: 1.9.5}
  approval: {by: [alice], identity: verified, source: github-environment production https://github.com/acme/infra/actions/runs/1234, digest: sha256:9f2c…}
  provenance:
    /variables/vpc_id: {unit: network, uid: 8c1f…, executionKey: sha256:…, observedCommit: 41f0…}
  outputs: {host: db.dev.internal, port: 5432}   # declared, non-sensitive only
  artifacts: []                                   # e.g. [{name: image, kind: ContainerImage, digest: sha256:…}]
desired:                             # the DesiredUnit the receipt was produced for
  unit: {environment: dev, name: database, uid: 5d2a…, apiVersion: units.gitops.nyl/v1, kind: OpenTofu}
  executionKey: sha256:…
  # … the full document, as written to desired/units/database.yaml by that run
condition: null
# or, when an operator or retry is needed:
# condition: {state: uncertain, since: 2026-09-24T11:12:00Z, run: 0b8f…, reason: run lost its lease}
# condition: {state: failed, since: …, run: …, category: tool-error, retryable: false, message: "tofu apply exited 1"}
```

- A receipt is **current** when its unit uid and execution key equal those of
  the unit's current desired document (owner:
  `nyl_orchestration::rules::freshness::receipt_is_current`). The execution
  key alone decides: producer identity, such as the uids its provenance
  cites, never makes a receipt stale and is not part of the execution key. A
  unit that must run again when a producer is recreated encodes that in its
  inputs, for example by consuming an output that changes with every
  creation. When the desired document changes, the receipt remains until the
  next execution replaces it; it documents what ran but satisfies no
  reference.
- `desired` is a copy of the desired document the receipt was produced for,
  written by the same transition that writes the receipt (a run, a takeover
  import, or `recover --accept`) and absent without a receipt. A hold reverts
  to it directly, so no decision reads Git history for it (see
  [Deletion](#deletion)).
- `waiting` records a two-phase publication teardown that published phase 1
  and waits under its `teardownWait`: `phase`, `reason`, `since`, the
  advisory `until`, `phase1Commit`, and the `run` that published it (see
  [Kubernetes publication unit](#kubernetes-publication-unit)). It persists
  between runs until phase 2 completes.
- `condition` records the latest failure or uncertainty and stays until the
  unit next succeeds or an operator runs `recover`. A condition does not remove
  the last receipt.
- A new receipt removes the unit's drift record in the same commit, because
  the record was about the receipt it replaces (owner:
  `rules::drift::drift_changes`).

### Attestations

```yaml
# observed/units/kubernetes/attestations.yaml
apiVersion: gitops.nyl/v1
kind: AttestationRecord
unit: {environment: dev, name: kubernetes, uid: 91c3…}   # absent in observed/attestations.yaml
attestations:
  healthy:
    executionKey: sha256:…            # the receipt attested; environment-wide: the desired commit of the state
    result: pass                      # pass | fail
    at: 2026-09-25T16:40:00Z
    source: {driver: attest}          # or {driver: reconcile}, or {external: {by: alice, source: …, url: …}}
    reason: null
    details: {}                       # driver-specific, such as each Application's running publication
```

- Drivers report the attestations a unit declares from its resolved spec,
  besides `spec.attestations` entries with `from: external`; an Environment's
  `spec.attestations` declares environment-wide ones. The declared set is part
  of the desired unit, so a receipt knows what it can be attested for.
- A driver attests during `reconcile`, as part of its outcome, or when
  `nyl attest` asks it to, which observes the system afterwards, such as a
  publication's Argo CD Applications. Attestations are evidence for
  promotion and dependents; they are not drift, which says whether a unit
  must execute again (see [Drift](#drift)).
- `nyl attest -e <env> [--unit <u>]` asks the driver of every unit with a
  current receipt whose kind attests for its spec (the `attest`
  capability), and records the results with `source: {driver: attest}` and
  the driver's `details`, such as each Application's running publication
  and health, in one observed commit; it writes nothing when nothing
  changed, where an attestation whose result is unchanged counts as unchanged
  until it is older than the refresh interval (default 15 minutes), after
  which it is written again with its new time
  (`rules::checks::settle_attestations`), so a re-verified system stays fresh
  for `maxAttestationAge` and an unreachable one does not. Units whose kind does not attest are left out; one named with
  `--unit` is reported `unsupported` (`NYL-ATTEST-UNSUPPORTED`).
- `nyl attest --name <n> --result pass|fail` records an external
  attestation (see the [promotion contract](promotion.md#health-evidence)).
  Both forms run under the lease, like every state-writing operation, and
  `--wait-lease` lets a CI job wait for a running reconcile; a refused name
  exits 1. Implementation status: the driver form is implemented, including
  the publication unit's `accepted` and `healthy`; the external form is not
  yet.
- An attestation is valid only for the receipt or state it names. Declared
  attestations of a new receipt are pending until attested; the newest per
  name and receipt wins. Git history keeps earlier ones, so an older state
  keeps the attestations it had. A transition that records a new receipt drops
  the attestations of the receipt it replaces in the same commit
  (`rules::evidence::attestation_changes`), and a recorded attestation that
  names another receipt or incarnation never counts.
- `evidence: attested` requires every declared attestation of the producer to
  pass; `attestations: [<name>, …]` requires the named ones instead, whatever
  `evidence` says, and `evidence: published` with no names requires only the
  receipt. A producer that declares none satisfies `attested` with its current
  receipt. A failing attestation fails that requirement until a newer one
  passes. Dependents (`dependsOn` and `fromUnit`, in this environment or
  another declared one) apply these rules to the producer's current receipt as
  [Units](#units) describes; a dependent does not age attestations, since a
  recorded pass for the current receipt satisfies it, and `maxAttestationAge`
  belongs to promotion paths. Promotion applies the same rules per unit and
  for the environment, as the [promotion contract](promotion.md#promotion-paths)
  describes.
- A driver's `reconcile` outcome attests as well: the names it reports that
  the unit declares are recorded with `source: {driver: reconcile}` in the
  run's transition commit, for the receipt the execution wrote, and are
  visible to the consumers of a later wave of that run. A declared name it
  leaves out stays pending. Run checkpoints do not carry them: a receipt that a
  later run imports from a dead run, or that `nyl recover --accept` adopts, has
  its attestations pending until `nyl attest` or a new execution records them.

**Consumer freshness.** A reference is satisfied only by a current producer
receipt. When a producer's desired document changes, its old receipt stops
being current, and every consumer that resolved from it becomes `blocked` on
that producer, even if the consumer already has its own receipt. The
consumer's existing receipt still documents what ran; the consumer executes
again only after the producer's new receipt exists and re-resolution changes
the consumer's execution key. A producer recreated with a new uid and the same
outputs therefore leaves its consumers current: re-resolution cites the new
receipt and the consumers' execution keys stay the same.

The same holds for a consumer that only has a `dependsOn` entry, with or
without required attestations: the entry orders the units and gates the
consumer on the producer's current receipt and attestations, but contributes
no value to the consumer's execution key. A producer's new receipt, once
attested, therefore leaves such a consumer `current`; it does not run again.
A smoke test that must run against each new version of its producer consumes
something that changes with it, such as `fromUnit` of an output or artifact
field that carries the version, image digest, or endpoint, whose changed value
changes the consumer's execution key. It also keeps the evidence gate: with
`attestations` on the same reference, the new value is read only once the new
receipt's attestations pass.

### Observed values

An observed value is a value the unit's driver reads from the system the unit
manages, such as the address a cloud load balancer assigns to a published
Service. Unlike an output, executing the unit does not produce it: it can
appear after the execution and change while the execution key stays the same.
It is therefore an observation of the current receipt, never part of the
receipt (owner: `nyl_orchestration::rules::observed_values`).

```yaml
spec:
  observations:
    ingressIp:
      type: string           # checked by Nyl
      maxAge: 1h             # optional
      from:                  # the kind's source, owned by its driver; here a KubernetesPublication's
        resource: {apiVersion: v1, kind: Service, namespace: ingress, name: ingress-nginx}
        pointer: /status/loadBalancer/ingress/0/ip
  observationWait: 10m       # default 0s: one attempt, then later runs
```

- `observations` declares each observed value with a `type` from the Release
  input type set, an optional `maxAge`, and `from`, the kind's source for it.
  Nyl owns `type` and `maxAge`; `from` belongs to the driver, which supplies
  its schema and validates it, and Nyl never reads it. A name may not also be
  a declared output. Observed values are
  never sensitive: a declaration with `sensitive` is rejected, because a value
  that cannot be recorded cannot be referenced. Observed values are committed in
  plain text to the state repository, so a unit observes only non-secret
  fields; a driver rejects the sources it can tell are sensitive. A kind whose driver lacks the
  `observed_values` capability rejects `observations` with
  `NYL-OBSERVATIONS-UNSUPPORTED`.
- **Reading.** The driver's `observe` reads each declared observation for the
  unit's current receipt and reports a value, `pending` with a reason (the
  system has not caught up with the receipt, or the field is not set yet), or
  `failed` with a reason (it could not read, such as missing credentials).
  The driver decides when a value belongs to the current receipt; the engine
  checks it against the declared type and never interprets the reason.
- **Lifetime.** A recorded value belongs to the receipt whose execution key and
  run it names. A new receipt, a re-execution with the same key included,
  starts with every observation `pending` and removes the
  previous receipt's values in the same commit, as for a drift record, so
  a value read from what ran before can never satisfy a reference.
- **When Nyl observes.** `reconcile` observes every selected unit with a
  current receipt and declared observations once, before it resolves the
  unit's dependents, including a unit it does not execute; this is how a
  value that changed by itself reaches its consumers in one run.
  `nyl observe -e <env> [--unit …]` reads and records observed values
  without executing anything, for jobs that hold the read credentials a
  reconcile may lack, such as access to a workload cluster that publication
  does not need. It covers the units whose kind has the `observed_values`
  capability and declares observations, takes the lease, writes one observed
  commit (operation `observe`), and shares the check path of `detect-drift`
  and `attest`: `no-receipt`, `unsupported` for a unit `--unit` names whose
  kind cannot observe, `pending` and `failed` per observation, and exit 0
  when every observation has a value, 2 when one is `pending` or `failed`.
  `plan` and `status` read only recorded observations. The report is an
  `ObserveReport` whose units carry `check: observed` with each observation's
  read (`value` with the value, `pending` or `failed` with the reason); a unit
  that executes nothing here is `current` in `status` until a consumer's
  stored resolution differs from the recorded value. `nyl get observation
  <unit>/<name>` prints the recorded value, resolved like a `fromUnit`
  reference.
- **Changed values are work for the next reconcile.** An observation that
  changed is neither drift nor evidence: the producer's receipt still holds.
  Its consumers' resolved inputs changed, though, so `plan` and `status`,
  resolving from the recorded value, report each consumer `ready` with the
  observation that changed as the reason (status exits 2), and `nyl observe`
  names them with `NYL-OBSERVATION-CHANGED`, whose fix is the next
  `nyl reconcile`. A consumer that blocked on a `pending` value that now has
  one is `ready` the same way ("arrived"); both are found from the stored
  desired documents, without rendering. They name consumers of the
  same environment only: a consumer that reads another environment's value
  through `environment` is not named by `observe` or `status` of the producer
  or of its own environment, and runs when its own next `reconcile` resolves
  the producer environment's recorded value. That reconcile executes them, under
  their own `approval`, as for any changed input.
- **Waiting.** After the run executes a unit, a `pending` observation is
  polled under the run's lease until it has a value or `observationWait` has
  passed since the execution finished, within the unit's `timeout`; the run
  checks about every 15 seconds, with a last check that starts at or after the end of the wait, and
  computes the end from the injected clock. The run waits only when no
  other unit is ready, so units that do not depend on the observation are not
  held back. With the
  default `0s`, the run makes one attempt and the observation stays `pending`
  for a later run. A unit the run did not execute is observed once, without
  waiting.
- **Unavailable reads.** When a read `failed`, the last recorded value for the
  current receipt stays usable and the run warns with
  `NYL-OBSERVATION-FAILED`, naming the driver's reason. An observation
  declared with `maxAge` stops resolving once its last successful read is
  older than that; it is then `pending` for its consumers.
- **Recording.** A run or `nyl observe` writes the unit's observation record
  (`observed/units/<unit>/observations.yaml`, see
  [State layout](#state-layout)) only when a value or its `pending` state
  changes, or when the last successful read is older than the refresh
  interval, an hour by default (half of the observation's `maxAge` when that
  is shorter), so `maxAge` stays satisfiable without a commit per run. A
  `pending` read of an observation that is already `pending` changes nothing,
  whatever its reason. The transition commit lists, per unit, the
  observations whose value arrived, changed, or went `pending` under
  `observationsChanged` (`{lb: [ip]}`); a unit's first `pending` record, and
  a re-read of the same value, are no change.
- **Consumers.** `fromUnit: {unit, observation, pointer}` resolves the
  recorded value; the producer's receipt must be current with the required
  `evidence`, as for an output. A `pending` observation blocks the consumer,
  exit 2, with `NYL-OBSERVATION-PENDING` and the driver's reason. The resolved
  value enters the consumer's execution key, so a changed value makes the
  consumer execute again like any changed input, under its own `approval`;
  `driftRepair` does not apply, because the consumer's inputs changed rather
  than its resources. Provenance cites the producer's receipt, the observation
  name, its `observedAt`, and the observed commit. A run resolves each
  observation once, so all its consumers see the same value.
- Cross-environment references read observations like outputs, from the
  producer environment's observed tip. PromotionPath selectors do not select
  observed values yet; an `input` selector reads a consumer's input resolved
  from one like any other input (see
  [PromotionPath](promotion.md#promotionpath)).
- The walkthrough [`observed-value`](#walkthroughs) shows a value arriving
  after a publication, a run that waits for it, a change that reaches its
  consumer without the producer executing, and `nyl observe` leaving the
  consumer `ready` for the next reconcile.

### Transition commits

Human review of the state refs is a goal, so each operation writes at most one
commit per state ref: one reconcile, one promotion, one teardown, one drift
detection, one attestation.
The commit is the complete transition, and an operation that changes nothing
writes nothing. The per-unit progress of a run lives on a disposable run ref
instead (see [Runs and leases](#runs-and-leases)).

The commit message carries a summary that machines parse, followed by Git
trailers:

```text
nyl: reconcile dev (2 executed, 1 failed, 1 blocked)

units:
  network:    {result: current}
  database:   {result: executed, executionKey: sha256:…, approval: {by: [alice], identity: verified, source: …, digest: sha256:9f2c…}}
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

- Unit results are `current`, `executed` (with `executionKey`), `imported`
  (with `executionKey` and the `run` whose checkpoint became the receipt),
  `recovered` (with `executionKey`: an uncertain unit this run executed again
  under `converge`, or whose effects `inspect` found applied), `failed`,
  `uncertain`, `blocked`, and `awaiting-approval`, and for lifecycle work and
  drift: `removed` (torn down, its files removed), `retained` (a new
  tombstone), `released` (resources left in place, with what was left),
  `held` (with the pending source commit, and whether a teardown request is
  recorded on it), `drifted` (a current, drifted unit the run did not
  repair), `pending-teardown` (with the reason), and `tearing-down` (with the
  reason, such as a teardown wait). In a reconcile or teardown, `held`,
  `drifted`, `pending-teardown`, and `tearing-down` exit 2, except a held unit
  whose teardown request this invocation recorded, which exits 0; `removed`,
  `retained`, and `released` exit 0 (owner: `nyl_orchestration::run_report`).
  The headline counts `executed`, `recovered`, `imported`, `removed`,
  `retained`, `released`, `failed`, `uncertain`, `held`, `drifted`,
  `pending-teardown`, `tearing-down`, `blocked`, and `awaiting-approval`, in
  that order, leaving out zero counts.
- `driftRepaired` lists the units the run executed again because their
  latest drift record was `drifted`, and `reapplied` records
  `{by, at, reason, units}` of a `--reapply`; each appears only when not
  empty.
- `observationsChanged` lists, per unit, the observed values whose recorded
  value or `pending` state the operation changed; it appears only when not
  empty.
- Lifecycle work adds its specifics under `details`: `retained`, `released`
  (each unit with what it left in place), `allowDependents` (each unit with
  the tombstones the override passed), `confirmedRemoval` (each unit with
  `{by, reason, from}` of a confirmation, or `observed`, `not-observed`, or
  `incomplete`), `incomplete` (each unit with what may remain),
  `sourceOverride` (a teardown's `--source`), `teardownRequested` (held units
  whose teardown request the run recorded), and `held` (units a hold signal
  held during the run).
- `imported` lists the units whose checkpoints, left by a run that died or
  lost its lease, became receipts in this run. `recovered` lists the units a
  takeover gave an `uncertain` condition, because a dead run was executing
  them without a checkpoint or left a superseded result with real effects,
  and the units this run recovered by executing or inspecting them. A unit
  the takeover marked whose policy is `manual` is listed too; it keeps its
  condition and waits for `nyl recover`.
- `Nyl-Read-Desired` and `Nyl-Read-Observed` name the state commits the
  operation started from. `Nyl-Evaluated-At` is the time used for time-based
  decisions such as lease expiry.
- Operator actions add `Nyl-Requested-By` and `Nyl-Reason`; approvals are part
  of the summary and of each receipt.
- Operations: `reconcile`, `promote`, `teardown`, `hold`, `resume`,
  `recover`, `detect-drift`, `attest`, `observe`, `exec`, `ci-admit`, `state-init`,
  `state-renew`, `state-move`, `state-delete`,
  and `state-forget`. `lease break` writes only the lease ref; the
  transition commit that imports the broken run lists the break under
  `leaseBroken` with who broke it and why.
- Replaying operations in order, from the read commits and evaluation times
  they name, reproduces every state decision.

## Execution

The shell emits typed start and end events for dispatched units, using the
injected clock and the same normalized result and message as the unit report.
The CLI prints them on stderr, including the unit address, elapsed time, and
short failure reason. Public native-tool transcript lines are indented and carry
the unit address after secret masking. `--quiet-unit-output` hides those lines
without hiding lifecycle events; `--unit-log-dir` independently appends masked
lines to local per-unit files. A log open or write failure emits `NYL-UNIT-LOG`
and preserves the native tool result. Captured bytes and private stdout remain unchanged. Reports
keep a keyed `units` map and a `unitOrder` list: dispatch order first, then
undispatched units in dependency order, or consumers first for teardown. Names
break ties within a wave. Machine reports alone go to stdout.

The optional [CI execution adapter](ci-execution.md) exposes the same dependency
graph as separate jobs. Its first backend invokes ordinary unit-scoped operations:
each job is a separate run, acquires the environment lease, and revalidates
current evidence. CI success never replaces a receipt. The full ownership set
is resolved in every invocation; changed-path selection must not imply deletion.
This backend does not distribute one run or relax the lease. M3's graph interface
must represent blocked references before their values exist, so export does not
depend on a complete native plan.

M8's [deployment admission](ci-execution.md#deployment-admission) is shared by
all CI providers and opt-in per environment with `admission: required`. The
admission record is state, not a CI artifact, and only a newer admission
replaces it; `nyl ci admit` writes it under the lease, resolving S by the
environment's source rules and refusing an older or diverging source unless
it is an authorized rollback. A worker presenting `--generation` acquires the
lease and compares its generation with the record before resolution writes,
result import, or driver calls. A superseded generation executes nothing,
writes no desired/observed state, and reports `superseded`. Holds, recovery,
attestations, lifecycle work, and local runs change unit state, not deployment
intent, so they never touch the admission. Environments without
`admission: required` keep M3 semantics.

CI planning remains ordinary read-only planning, including the unfiltered
leaving section and the caller's `--fail-on-leaving` policy, and needs no
admission. CI reconciliation hands units leaving the ownership set to one
lifecycle job running `reconcile --leaving` after the unit jobs; the deletion
rules below apply unchanged.

`nyl reconcile -e <env>`:

1. **Lease.** Take the environment's run lease (see
   [Runs and leases](#runs-and-leases)).
2. **Resolve.** Render the environment's units at source commit S and resolve
   references against current receipts and PromotionRecords, then import
   results left behind by earlier runs against that resolution.
3. **Select.** A unit is ready when its desired document is `ready` and
   `active`, it has no current receipt, its provenance receipts are all still
   current, and it is not awaiting approval. An `uncertain` condition makes a
   unit a candidate whatever its receipt, because its effects may differ from
   what the receipt records; it is selected only when its driver's recovery
   policy allows it: `converge` executes it again, and `inspect` runs
   `inspect` first (see [Recovery](#recovery)). Under `manual`, the unit waits
   for `recover`. A unit with a non-retryable `failed` condition is not
   selected until `recover --retry` clears it. Freshness is decided by the
   execution key alone (see [Observed unit](#observed-unit)). A unit with a
   current receipt is a candidate again when its latest drift record for
   that receipt is `drifted`, or when `--reapply` names it; the drift rules
   decide whether it is selected or awaits approval (see [Drift](#drift)).
   Held units are never selected (see [Deletion](#deletion)).
4. **Wave.** Execute ready units, independent units in parallel up to a
   concurrency limit, and checkpoint each result on the run ref. Each
   concurrency slot keeps one worktree for the run and resets it to the next
   unit's effective commit, removing untracked and ignored files and
   rebuilding the checkout's repository metadata, so a unit sees exactly what
   a fresh checkout holds; the symbolic-link check of a commit's tree runs
   once per tree. Planning uses the first slot.
5. **Re-resolve.** New receipts may unblock dependents. Resolve again at the
   same S.
6. **Repeat** from step 3 until nothing is ready. Before it starts each unit,
   the run reads its signal ref and applies every `hold` it received, so it
   starts no held unit (see [Deletion](#deletion)).
7. **Lifecycle.** After the execution waves, units leaving the ownership set
   and units whose teardown was requested are torn down, retained, released,
   or dropped in reverse dependency order (see [Deletion](#deletion)).
8. **Commit.** Write one transition commit per state ref, fenced by the lease,
   then release the lease and delete the run ref. After a takeover, the run
   then consumes the leftover run refs it imported (see
   [Runs and leases](#runs-and-leases)).

A run never reads a source commit newer than S; later source changes are
picked up by the next run.

**Stopping early.** By default a run continues with everything it can
reconcile: a unit that fails, becomes uncertain, waits for approval, or is
blocked does not stop units that do not depend on it. With `--fail-fast`, on
`reconcile` and `teardown`, the run starts no further unit after the first
failure, uncertain outcome, or approval wait, including one decided when
units are selected, such as a drifted unit whose recovery policy is not
`converge` or a teardown that needs manual approval; units already running
finish.
It still checkpoints what finished and writes its transition commit. The units
it did not start report `blocked` with the reason `--fail-fast stopped the
run after <unit>`, and the run warns with `NYL-RUN-STOPPED` (owner:
`nyl_orchestration::shell::run`).

**Who writes desired state.** Desired state is derived deterministically from
source and observed evidence, so the reconcile runner writes it. Review happens
on source commits (the pull requests that change units and bindings) and on
promotions (`changeGate: pullRequest` opens a pull request against the desired
ref). Separate refs let that promotion review and different branch protection
apply to desired state without mixing with observed commits.

**Selection with `--unit`/`--units`:**

- `--unit <u>` is repeatable; `--units <u>,<u>,…` is the same list in one
  argument. Unknown names are an error, exit 1.
- Resolution always covers the whole environment.
- Only the named units execute. A dependency without a current receipt leaves
  the named unit `blocked`; dependencies and dependents are not executed.
- `plan --unit` resolves the whole environment too and reports only the named
  units, plus the leaving section, which is never filtered.
- `--leaving` selects exactly the units with state outside the ownership set:
  newly leaving, `deleting`, `pending-teardown`, or `tearing-down`. Lifecycle
  work can then run on its own after unit-scoped runs. It excludes `--unit`.
  A `--unit` run does no deletion work at all.
- `--reapply` executes the named units again although their receipts are
  current (see [Drift](#drift)). It is a flag that requires `--unit` and
  `--reason`; the names go through the same validation as every `--unit`.

**Transition commit conflicts.** Operations on one environment are serialized
by its lease, and the final push is fenced by it (see
[Runs and leases](#runs-and-leases)), so a push that still holds the lease
loses only to a commit made outside Nyl, typically a merged promotion pull
request, or to another environment sharing the same ref through `state.path`,
whose paths never overlap. Nyl then fetches
and rebases its commit if the winning commits
changed none of the paths it writes or read. Otherwise it keeps its results,
which are facts, resolves again against the new state, and commits that. When
desired and observed are separate refs, both commits and the lease check go in
one atomic push, so a desired conflict rejects the whole push; the results stay
on the run ref meanwhile, so no evidence is lost.

### Approval

```yaml
spec:
  approval: auto                       # default
  # or
  approval: {mode: manual, bind: plan} # bind: desired | plan
```

- A manual unit executes only when the invocation approves it. `approval:
  manual` is short for `{mode: manual, bind: desired}`.
- `bind: desired` approves one execution of the desired document the unit had
  when the invocation started, identified by its execution key: `--approve
  database`. If re-resolution later in the same run changes the key, the new
  document is not approved and the unit waits for a new approval, exit 2.
- `bind: plan` approves exactly the changes a reviewed plan showed. It requires
  a kind whose `plan` reports a change digest (Terraform and OpenTofu do).
  `nyl plan` prints each manual unit's digest, and `--approve
  database=sha256:…` authorizes that digest. At execution the driver plans
  again in the same run and applies that plan only if its digest matches;
  otherwise the execution ends without effects and the unit waits for a new
  approval. Plan files never leave the runner.
- `--approve` for a unit excluded by `--unit`/`--units` is an error.
- Teardown of a manual unit needs approval the same way; for `bind: plan` the
  digest is that of the destroy plan. `nyl plan` covers `deleting` units with
  teardown intent and prints their destroy-plan digest, and `nyl teardown`
  accepts `--approve` like `reconcile`. Without a matching approval it is
  reported `awaiting-approval`, and the run exits 2.
- The approved digest reaches the driver through `ExecutionContext`, and the
  driver compares it with the plan it is about to apply. A mismatch returns
  `Outcome::AwaitingApproval` with the new digest: no effects, the unit waits,
  and the run exits 2.
- `--approve <unit>` without a digest also approves a `bind: plan` unit: it
  authorizes whatever plan that run computes. The receipt and the transition
  commit record the approval as unreviewed (`digest: null, reviewed: false`),
  so the audit trail shows that nobody saw the exact changes.
- `--approve-all` approves every manual unit the invocation would otherwise
  stop at, recorded as unreviewed in the same way, for example
  `nyl state delete -e dev --teardown --approve-all --reason "decommission
  dev"`. Explicit digests combine with it: `--approve-all --approve
  database=sha256:…` pins that unit to a reviewed plan.
- `approval: {mode: manual, bind: plan, requireDigest: true}` refuses
  approvals without a digest, for units such as a production database. Both
  `--approve <unit>` without a digest and `--approve-all` skip such a unit and
  name it, and the run exits 2 until its digest is given.
- `--confirm-removed` is not an approval, and `--approve-all` does not imply
  it: approving a plan and asserting that workloads are gone are different
  claims.

Recording. Each approval is recorded in the receipt and in the transition
commit's summary: who approved (`by`, a list, because a gate may require
several reviewers), where (`source`), the digest, and how Nyl knows the
identity (`identity`). The identity is an audit record, not authorization: the
CI system decides who may run a job that passes `--approve`, and Nyl records
honestly how it learned who did.

| `identity` | Meaning |
| --- | --- |
| `verified` | Read from the CI system: a built-in lookup or the project's lookup hook |
| `asserted` | Passed by the invocation with `--approved-by` |
| `local` | The Git user of a workstation run |

- `--approved-by` and `--approval-source` set the identity and source
  explicitly and are recorded as `asserted`; they remain available with every
  CI system. Without them, a CI run uses a lookup when one applies and
  otherwise records the CI run URL as the source with no approver, and a
  local run records the Git user.
- A lookup hook covers CI systems without a built-in lookup: `[approval]
  lookup = ["./ci/approvers.sh"]` in `nyl.toml` names a command that Nyl runs
  with `NYL_ENVIRONMENT`, `NYL_APPROVAL_UNITS` (a JSON array of explicitly
  approved names), and `NYL_APPROVAL_DIGESTS` (a JSON object of names to
  digests or null) in environment variables and that prints `{"by": [...], "source": "...", "url": "..."}`. Nyl records
  the result as `verified`, naming the hook. The hook is optional, and it is
  trusted as much as the project's CI configuration, where the approval gate
  itself is defined. A failing or invalid hook fails the invocation before any
  effect, exit 1.
- `nyl attest` records attesters through the same mechanism.
- Approvals can be automated with a CI approval gate. With GitHub environment
  protection rules, a `plan` job runs `nyl plan --output json` and passes the
  digest as a job output; an apply job with `environment: production` runs
  `nyl reconcile --approve database=<digest>` only after GitHub's reviewers
  approve. When a GitHub token with read access to Actions is available, Nyl
  reads the run's approvers from GitHub's run-approvals API and records them as
  the approval's `by`, `verified`. GitHub is the only built-in lookup in M3; a
  GitLab lookup through protected-environment deployment approvals is planned
  for later, and until then GitLab and other CI systems use the hook or
  `--approved-by`.

```yaml
# .github/workflows/production.yaml (excerpt)
jobs:
  plan:
    outputs:
      digest: ${{ steps.plan.outputs.digest }}
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

Coordination and progress live outside the desired and observed refs, in the
same state repository, so one atomic push can cover a transition and its lease
check. They sit under `coordinationRefPrefix`, shown here with its default:

| Ref | Content | Lifetime |
| --- | --- | --- |
| `refs/nyl/<env>/lease` | One `Lease`: run ID, runner, operation, deadline, units executing | Created at run start, deleted at run end |
| `refs/nyl/<env>/runs/<run-id>` | The run's `Run` record, carrying the same deadline and units executing as the lease; `created/<unit>.yaml`, the desired document and uid of each unit the run created, written before the unit starts; and a checkpoint commit per finished unit: its receipt or failure, and its artifacts | Deleted after its results reach a transition commit; after a takeover, deleted or retired to an evidence ref by the run that imported it |
| `refs/nyl/<env>/signals/<run-id>` | `Signal` records other invocations send to a running run: teardown confirmations (see [Teardown](#kubernetes-publication-unit)) and holds (see [Deletion](#deletion)) | Deleted with the run ref: by the run after its transition commit lands, or by the run that imports it after a takeover |
| `refs/nyl/<env>/evidence/<run-id>` | A retired run ref: the `Run` record and checkpoints of a dead run that are still evidence for a unit with an `uncertain` condition, such as a late checkpoint `nyl recover --accept` may adopt | Created by a takeover in place of the run ref; deleted by a later takeover or by `nyl recover` once no `uncertain` unit needs it |

1. **Lease.** A run creates the lease ref with compare-and-swap. If a lease
   exists and has not expired, or if a run ref's `Run` record has a deadline
   that has not passed, the run exits 2 with `NYL-LEASE-HELD` and reports who
   holds it; a live run
   ref counts as a held lease, so a deleted lease ref alone never lets a
   second run execute units the first is still executing;
   `--wait-lease <duration>` waits up to that long for the lease instead, for
   jobs such as a preview's close job that must not give up. Every
   state-writing operation takes the lease (`reconcile`, `promote`,
   `teardown`, `hold`, `resume`, `recover`, `detect-drift`, `attest`, `observe`, `exec`,
   `ci admit`, `state init`,
   `state move`, `state delete`, `state forget`), so only one
   runs per environment at a time. `promote` takes the target environment's
   lease only. `lease break` is the one command that changes a lease it does
   not hold. A broken lease whose run no run has imported yet is taken over
   only by an operation that imports that run: `reconcile`, `teardown`, or
   `recover` (see item 8). Every other state-writing operation, `state forget`
   and `state delete` included, exits 2 with `NYL-LEASE-BROKEN`, or waits
   under `--wait-lease`, and names `nyl recover -e <env>` as the fix, so the
   dead run's effects and created units are recorded before anything else
   writes state. Read-only commands (`plan`, `status`, `get`, `state log`)
   never take the lease and work whatever its state, and neither does a
   `detect-drift` or `attest` with nothing to check, which writes nothing. `nyl hold` does not
   refuse a live holder: it delivers the hold to the running run through the
   run's signal ref (see [Deletion](#deletion)); a broken lease refuses it
   like every other non-importer.
2. **Deadline.** Every phase sets the deadline before it starts, plus a grace
   period that also absorbs clock skew, and no phase runs without one:
   resolution, including target renders and `fingerprint` scripts, by
   `resolutionTimeout` (`nyl.toml`, default 30 minutes); unit execution by the
   latest finish time of the units executing, by their `timeout`; `detect-drift`
   and `attest` by the timeouts of the units they check; a teardown wait by the wait's end.
   The runner updates the lease and the `Run` record together at each phase
   and unit start, with the batch's `created/` documents, in one atomic
   update that lands both or neither. There are no heartbeats.
3. **Checkpoints.** Each finished unit is checkpointed on the run ref before
   the next wave, together with the desired document it executed and its uid,
   including documents the run created. Before a unit the run created starts,
   its desired document and new uid are recorded under `created/` on the run
   ref, so a crash during its execution leaves the uid behind. A crash loses
   no evidence.
4. **Takeover.** A run that finds an expired lease replaces it with
   compare-and-swap. After resolving, it imports every leftover run ref. A
   checkpointed result becomes a receipt when the new resolution of that unit
   has the same execution key. The new run adopts the checkpointed desired
   document and uid of every unit the dead run created and started, with or
   without a checkpoint, instead of generating new ones, whether or not the
   new resolution still selects it: a unit no longer selected then leaves the
   ownership set like any other and, by default, becomes `pending-teardown`,
   so resources a crashed run created are never left without state. Other results are reported as superseded; their effects are real,
   so a superseded result of a unit whose recovery policy is not `converge`
   gives it an `uncertain` condition rather than letting it run again. Units
   the dead run was executing without a checkpoint get an `uncertain`
   condition. All of this lands in the new run's transition commit, under
   `imported` and `recovered`. A checkpoint that arrives after its unit was
   already marked `uncertain`, because a runner that lost its lease finished
   the unit late, never clears the condition by itself: later runs import it
   as evidence, and `nyl recover` shows it, with its receipt or failure, next
   to the condition; `nyl recover --accept <run>` adopts it (see
   [Recovery](#recovery)).
   - Import is idempotent and never regresses state. A takeover condition
     records the dead run's ID, and a unit whose condition already names that
     run is not marked again. A checkpoint applies only when it is newer than
     the unit's observed document, and a unit the dead run started is not
     marked when its observed document changed at or after that run's
     deadline. When several leftover runs touched a unit, the newest, by
     deadline and then run ID, decides, and older checkpoints are superseded.
   - A name fenced by a deleting incarnation (a kind change, or a replace)
     imports a leftover run's touches of it against that stored, deleting
     incarnation, not the new one in source, so a teardown the dead run
     started without a checkpoint gives it an `uncertain` condition rather
     than being lost with the run ref.
   - An adopted unit that the new resolution no longer selects is written with
     its checkpointed receipt, since there is no new key to compare, or,
     without a checkpoint, with an `uncertain` condition, so the transition
     commit records state for every resource a dead run created.
   - A superseded result with real effects, a receipt or an `uncertain`
     outcome, whose execution key differs from the unit's current receipt
     gives the unit an `uncertain` condition whatever its recovery policy;
     a converging unit then runs again in the same run. Superseded results are
     reported with `NYL-RUN-SUPERSEDED`, late checkpoints with
     `NYL-RUN-LATE-CHECKPOINT`.
   - The `units` of a replaced lease count as started only for operations with
     effects (`reconcile`, `teardown`, `exec`); a lease taken by `detect-drift`,
     `attest`,
     `recover`, or another operation without unit effects marks nothing.
   - After its transition commit lands, or when it releases the lease without
     changes, the run consumes the leftovers once: a run ref whose checkpoints
     are evidence for a unit that is still `uncertain` is retired to
     `<prefix><env>/evidence/<run-id>`, the other run refs and their signal
     refs are deleted, and so are evidence refs no `uncertain` unit needs.
     Evidence refs are never imported again and never count as a lease. The
     run first reads the coordination refs again and consumes only refs
     unchanged since the takeover read them, because the holder of a broken
     lease may still be alive and checkpoint in between; a changed ref is
     left for the next run, which imports what arrived.
   - `nyl recover` touches run refs only when it imports the run of a broken
     lease (see [Recovery](#recovery)). It writes a single transition commit
     and executes nothing, so it writes no `Run` record or run ref of its
     own: its phases renew only the lease. A `--local` run operates on the
     local coordination refs (see [Local runs](#local-runs)).
5. **Fenced commit.** The final transition commit is pushed with
   `git push --atomic`, together with a compare-and-swap of the lease ref that
   expects the run's own lease commit, so either both land or neither does.
   The push moves the lease ref to a new commit holding the same lease,
   because Git checks a lease only on a ref the push changes.
   Releasing the lease is a compare-and-swap too. A run whose lease was taken
   over pushes nothing to the state refs and exits 4 with `NYL-LEASE-LOST`;
   its results stay on its run ref for the next run to import. Atomic pushes work on GitHub, GitLab,
   and plain Git servers.
6. **Lost lease.** A runner whose lease update fails stops starting units,
   waits for the units still running, checkpoints them to its own run ref,
   marks its `Run` record finished, so the run ref no longer counts as a held
   lease, and exits 4 with `NYL-LEASE-LOST`. The next run imports those
   results as above.
7. **Cancellation.** On SIGINT or SIGTERM, a run stops starting units and
   forwards the signal to running tools' process groups, so tools such as
   OpenTofu stop cleanly and release their own locks; a tool still running
   30 seconds later is killed. The run checkpoints units that finish, gives
   units that were still running an `uncertain` condition, marks its `Run`
   record finished, so the run ref no longer counts as a held lease, pushes
   its run ref, and releases the lease. It skips the final transition commit
   and exits 4 with `NYL-RUN-CANCELLED`; a run cancelled before it
   checkpointed anything deletes its run ref instead. The next run imports the
   run ref as after a crash, without waiting for a deadline. A second signal
   ends the process at once, with exit code 128 plus the signal number, as a
   forced kill would; the next run takes over as after a crash. A signal also
   stops any operation waiting for a held lease (`--wait-lease`) with
   `NYL-RUN-CANCELLED`, having written nothing: `reconcile`, `detect-drift`, and
   `attest` exit 4, `recover` and the state operations exit 1, like their other
   refusals. `detect-drift` and `attest` on a signal start no further batch and releases the lease
   without writing an observed commit (exit 4); `recover` and the state
   operations finish their single commit once they hold the lease. CI systems allow
   only seconds between the signal and a forced kill, so this path does no
   other work. Pipelines should not cancel runs that apply changes; a CI
   concurrency group per environment serializes jobs without cancelling
   them.
8. **Breaking a lease.** When a holder died without releasing its lease, for
   example after a forced kill, `nyl lease break -e <env> --reason <text>`
   declares it gone by replacing the lease with one marked broken, naming who
   broke it and why. When only a live run ref holds the environment, because
   its lease ref is already gone, the broken lease names that run. The next
   run takes over at once, as after expiry, and
   units the holder was executing become `uncertain`. The command warns that
   a holder that is in fact alive may still be causing effects; the fenced
   final push keeps it from overwriting state either way. Only an operation
   that imports the broken run takes the broken lease over (item 1), and its
   transition commit records the break once, under `leaseBroken` with who
   broke it and why, next to the import it caused. A run that took a broken
   lease over and ends before its transition commit lands, such as a
   reconcile that fails during resolution, restores the broken lease when it
   releases, so the break stays in effect until an import is committed. Once
   the import lands, the break is cleared: a holder that is in fact alive
   notices the lost lease at its next update and stops, and its later
   checkpoints are imported by the next `reconcile` or `recover`, with
   `nyl status` showing `NYL-RUN-UNIMPORTED` meanwhile. When the broken run
   left nothing to import, the importing operation still writes a transition
   commit recording the break (owner: `nyl_orchestration::shell::lease`;
   the predicate shared by every importer is `rules::lease::unimported_break`).

`nyl status` reads the lease and run refs, so it shows a run in progress and
the units it is executing. A finished run ref that no reconcile has imported
yet, left by a cancelled run or one that lost its lease, is reported with
`NYL-RUN-UNIMPORTED`: the units it left uncertain, those its record still
names and those it checkpointed uncertain, read `uncertain` and status exits
4; a run that recorded every unit only warns.

The lease coordinates runs; it does not authorize them. Forges such as GitHub
cannot protect refs outside branches and tags, so anyone with push access can
delete a lease or run ref. State stays safe either way: every state change
reaches the protected state refs only through the fenced push, so a holder
whose lease was deleted exits 4 at its next lease update or final push. A
deleted lease alone does not start a second execution, because the holder's
run ref still blocks new leases until its deadline passes. Deleting both refs
while a run executes is deliberate misuse of push access, like running the
tools directly: the next run may then execute units the first is still
executing, and the first run's results are reported as superseded.

### Recovery

A driver can return `RetryRequired {category, message, context}`. It is recorded
as a non-retryable failure with `retry: {state: required, context}` on the
observed unit. Context is non-secret, driver-owned data; orchestration does not
interpret it or diagnostic text. `recover --retry` requires a nonempty reason
and records `retry: {state: acknowledged, context, request}` while clearing the
condition. Ordinary recovery and recovery after importing a run use the same
rule. The acknowledgement is bound to the observed unit incarnation; the
driver validates its context against the resource it will operate on. A
successful receipt consumes it; a failed attempt retains it.

An uncertain condition means effects may or may not have happened. Each driver
declares a recovery policy for execution and for teardown:

| Policy | Meaning | Initial drivers |
| --- | --- | --- |
| `converge` | Re-executing the same document is safe; the next `reconcile` executes it again | Terraform, OpenTofu, OciImage, KubernetesPublication, command with `idempotent: true` |
| `inspect` | The driver's `inspect` reports applied (with outputs), not applied, or unknown; applied records the receipt, not applied executes again, unknown leaves the condition in place and the unit waits for `recover`, as under `manual` | Drivers that can observe their effects |
| `manual` | Blocked until an operator runs `recover --retry` | Command units without `idempotent: true` |

`nyl recover -e <env> --unit <u> --retry [--reason <text>]` clears an uncertain
condition or a failure, and records the operator's decision and reason, such
as "verified nothing was applied", in its transition commit. A retryable
failure needs no recover, because the next `reconcile` retries it, but
clearing it is allowed. `recover` keeps the unit's last receipt. It does not
execute by itself: the unit becomes ready and runs in the next `reconcile`, so
an operator can decide locally while CI executes. `recover` refuses a unit
without a condition with `NYL-RECOVER-NOTHING`. Accepting an
uncertain execution as applied requires outputs, so it is possible only through
`inspect`, or through a checkpoint that recorded the execution's result:
`nyl recover -e <env> --unit <u> --accept <run> --reason <text>` adopts the
receipt, outputs, and artifacts that run checkpointed, typically after a
runner that lost its lease finished late. It is allowed only when the
checkpoint's execution key and uid equal the unit's current desired
document's (`NYL-RECOVER-KEY-MISMATCH` otherwise), refuses a run without a
checkpointed receipt of the unit (`NYL-RECOVER-CHECKPOINT-MISSING`), and
records the operator and reason in its transition commit. `--accept` looks
the run up on run refs and evidence refs, and `nyl recover` lists the
evidence, every checkpoint of the unit there, newest run first, with whether
`--accept` would adopt it.

`recover` takes the lease and writes one observed commit, with a desired
commit beside it when an import adopts units a dead run created or
`--accept` writes the receipt's desired copy, with the operation `recover`,
the unit under `recovered`, the action under `units`, such as
`database: {action: accept, run: 0b8f…}` or `{action: retry}`, a headline
such as `nyl: recover dev (database: accept 0b8f…)`, and the
`Nyl-Requested-By` and `Nyl-Reason` trailers. An importing recover's headline
gives the counts first, then the action after a semicolon, such as
`nyl: recover dev (1 imported, 2 recovered; database: retry)`. Every receipt and condition change, of a run, a
takeover, or `recover`, follows one transition table: a success or an
accepted checkpoint replaces the receipt and clears the condition; a failure
or an uncertain outcome keeps the receipt; an import onto an `uncertain` unit
is evidence only. Apart from importing a broken run, below, `recover` never
touches run refs; after its commit it deletes the evidence refs that no
`uncertain` unit needs any longer.

**Importing a broken run.** When the lease is broken and its run has not been
imported, `nyl recover -e <env>` takes the lease over and imports that run the
way a `reconcile` takeover does, through the same run path with execution
switched off (owner: `nyl_orchestration::shell::recover::recover_environment`):
it resolves the desired units, adopts the uids of units the dead run
created, turns checkpoints whose execution key matches into `imported`
receipts, and gives units the run left without a checkpoint an `uncertain`
condition. Nothing executes, so a converging unit made `uncertain` runs again
in the next `reconcile` under its recovery policy. It records the units as
they are: units that left the ownership set are marked `pending-teardown` by
the next `reconcile`, not by the import. `recover` writes no run record: it
reads the broken run's refs, writes its one transition commit, operation
`recover`, listing the import under `imported` and the break under
`leaseBroken`, and consumes the run ref as a takeover does (see
[Runs and leases](#runs-and-leases), item 4). If it crashes before its commit
lands, the broken run's refs stay for the next import. With `--unit`, the
`--retry` or `--accept` action applies after the import, in the same commit,
so an operator can break a lease, import, and decide a unit in one command.
Once the import is recorded, `recover` exits 0 and lists what it imported;
`nyl status` then exits 4 for the units left uncertain. Without `--unit` and
without a broken run to import, `recover` refuses with `NYL-RECOVER-NOTHING`.
Where no state can be read, because it is missing, moved, deleted by
`nyl state delete`, `nyl recover -e <env>` still takes
the broken lease over, records the break where state exists, and releases
it, so `state init` (`--fresh` for deleted state) or `state move` can run.

**Holds on uncertain units.** A hold on a unit with an `uncertain` condition
keeps the desired document and uid of the unit's last receipt as its desired
state, read from the receipt's `desired` copy, so consumers keep resolving
from that receipt and downstream work can proceed while the unit is
investigated. The condition and any evidence ref stay: runs report the unit
`held` (exit 2), `status` shows it held with its `uncertain` condition and
exits 4, and `resume` does not clear the condition, which still needs a run
under its recovery policy or `recover`. On a held unit, `recover --retry`
clears only the condition, and the unit runs after `nyl resume`; `recover
--accept` is refused, naming `nyl resume`, because a held unit runs nothing.
A takeover import of a held unit works as usual: a checkpoint matching the
held document becomes its receipt, other units are marked uncertain, and the
unit stays held. A unit without a receipt has nothing to fall back on, and
its dependents stay blocked (see [Deletion](#deletion)). The walkthrough
[`held-uncertain`](#walkthroughs) holds an uncertain producer and reconciles
a consumer against its previous outputs.

A retryable failure is retried by the next `reconcile`; a non-retryable one
waits for `recover --retry`. A failure is a claim that the execution made no
effect it did not report; when effects may have happened, the result is
uncertain instead. The failure categories are closed, so reports and exit
categories treat them alike across drivers:

| Category | Meaning | Retryable by default |
| --- | --- | --- |
| `tool-error` | The tool ran and reported failure, such as a nonzero exit or a failed apply | Per kind: Terraform and OpenTofu yes; OciImage yes; Command only with `idempotent: true` |
| `output-rejected` | Outputs or artifacts violate their declarations: missing, mistyped, or sensitive but not declared so | No |
| `missing-tool` | A required binary is absent or reports another version than the spec pins | Yes |
| `credentials` | An admitted variable or secret is missing, or the tool rejected its credentials | Yes |
| `ownership` | The execution found resources it must not touch, such as a publication file changed outside Nyl or a native lock held by another run | No |
| `timeout` | A plan, drift detection, or attestation passed its deadline; executions and teardowns that pass their deadline are uncertain instead | Yes |

A driver may set `retryable` explicitly for a single failure; the defaults
apply otherwise.

#### Drift

A receipt proves that an execution succeeded for its inputs; it does not prove
that the resources still match, because a change made outside Nyl leaves the
execution key unchanged. `nyl detect-drift` asks each driver whether a current
receipt still holds and records the answer as the unit's drift record
(`observed/units/<unit>/drift.yaml`), tied to that receipt's execution key.
Drift is a reason to act, not evidence: it decides whether a unit executes
again, while attestations are evidence for promotion and dependents (see
[Attestations](#attestations)). Nyl does not watch for drift itself: a
pipeline runs `nyl detect-drift` on a schedule, followed by `nyl reconcile`,
until a continuous runner (M7) takes over the cadence. A planned opt-in
`nyl reconcile --detect-drift` (M7) does both under one lease and in one
transition commit; without it, reconcile never detects drift.

- **What it checks.** It checks every unit of the environment, or those
  `--unit` names. A unit without a current receipt is reported `no-receipt`
  (`NYL-DRIFT-NO-RECEIPT`), a kind that cannot detect drift `unsupported`
  (`NYL-DRIFT-UNSUPPORTED`), and a driver that cannot tell `unknown`
  (`NYL-DRIFT-UNKNOWN`). It writes one observed commit with the changed drift
  records, nothing when only the time changed, and never a receipt. Owner:
  `rules::checks`.

- **`drifted` is a unit state.** A unit with a current receipt whose latest
  drift record for that receipt is `drifted` is work for the next
  reconcile, like a changed unit: `nyl status` shows it `drifted` and exits 2
  for it, as for `ready`, and `nyl plan` lists it as a unit that would
  execute. Owners: `rules::selection` decides selection, `rules::drift`
  names repairs and the status attribute, and `rules::unit_state` the state.
- **Automatic repair.** With `driftRepair: converge`, the default, the next
  `reconcile` executes again every drifted unit whose recovery policy is
  `converge`, as it would execute a unit whose inputs changed. Its result is
  `executed`, its transition commit lists it under `driftRepaired`, and the
  new receipt replaces the one the record was about and removes the record
  in the same commit, so it is not repaired again until a later
  `detect-drift` finds new drift. Idempotent units therefore repair drift without
  an operator.
- **Units that do not converge.** Under `driftRepair: converge`, a drifted
  unit that is not safe to re-run is reported `awaiting-approval` (exit 2)
  while the rest of the run continues. `--approve <unit>` executes it,
  recording the invocation's approval identity.
- **`driftRepair: manual`** turns automatic repair off for an environment,
  such as production where every repair is reviewed: reconcile repairs no
  drift by itself, and drifted units stay `drifted` (exit 2) until a forced
  run. An EnvironmentGroup may set `driftRepair` for its members (see
  [Environment groups](#environment-groups)).
- **Forced runs.** `nyl reconcile -e <env> --unit <u> --reapply --reason
  <text>` executes the named units again although their receipts are
  current, drifted or not, and records `{by, at, reason, units}` in its
  transition commit under `reapplied`. `--reapply` is a flag that requires
  `--unit` and `--reason`; the names go through the ordinary `--unit`
  validation. A named unit whose recovery policy is not `converge` follows
  the approval rule above: `awaiting-approval` without a matching invocation
  approval, executed with `--approve`. It never fails the whole run. `nyl plan`
  has no `--reapply`.
- **Drift warnings.** `nyl detect-drift` reports each drifted unit with
  `NYL-DRIFT-DETECTED`, whose fix says, per unit, what the next reconcile does:
  it repairs the unit, the unit awaits approval, or, under `manual`, it needs
  `nyl reconcile --unit <u> --reapply --reason <text>`.
- The walkthrough [`drift`](#walkthroughs) shows `detect-drift` finding drift,
  `reconcile` repairing the idempotent unit, and the other awaiting
  approval.

#### Operator exec

Some repairs need a unit's native tool: moving or importing native state,
releasing a native lock, or relocating a state backend. `exec` is a driver
capability like `plan` or `teardown`; which tool it runs, and how the driver
prepares it from the spec, is the driver's implementation:

```text
nyl exec -e <env> --unit <u> [--worktree] [--timeout <duration>] [--reason <text>] -- <tool arguments>
```

- **Spec.** Without `--worktree`, the unit's current desired document, which is
  what is deployed. With `--worktree`, the unit rendered from the entry
  worktree, uncommitted edits included, as `nyl build` renders it, and resolved
  against current receipts. Changing a setting for a repair, such as an
  OpenTofu backend, a tool version, or a variable, is an edit to the unit
  definition, the same edit that is then reviewed and merged; `exec` has no
  kind-specific options. The rendering must keep the unit's name and kind.
- **Source files.** With `--worktree`, every source field that resolves to the
  entry repository at S, the default, reads from a snapshot of the entry
  worktree instead of a checkout of S: the Git-visible files `nyl render`
  sees, uncommitted and untracked ones included. The snapshot is copied into
  the execution's own directory, so the tool never writes into the operator's
  checkout, and temporary edits to the applied source, such as an `import` or
  `removed` block, reach the tool. A source pinned to its own `revision` or
  `commit`, or to another repository, is read at that pin; repairing with edits
  there means pointing the pin at a pushed repair branch in the unit edit,
  under the ordinary reachability rules.
- **Guards.** The unit must be held, so no run reconciles it during the repair,
  and `exec` takes the environment lease like every state-writing operation,
  with a deadline from `--timeout`, defaulting to the unit's `timeout`. The
  ownership scopes of the spec must not overlap another unit's, so a changed
  setting cannot point the tool at state another unit owns. Credentials are
  the spec's admitted `env`, so a credential change is also a recorded edit.
- **Tool.** The driver prepares the tool from the spec in the execution's own
  worktree and temporary directory, then runs it with the arguments after
  `--` unchanged, through the context's process handling and masking. Stdin and
  the terminal are passed through only when the driver declares the tool
  interactive and Nyl runs on a terminal.
- **Record.** The transition commit records the operator, reason, tool
  arguments, the tool's exit status, and the spec used: the desired document,
  or the worktree's commit, whether it was dirty, the spec digest, the fields
  that differ from the desired document, with values only where they are not
  sensitive, and the snapshot's digest with the paths that differ from that
  commit. It never records file contents, secrets, or tool output.
  Nyl first writes an observed transition with the operator-modified marker,
  before the tool can change native state, then records the tool status in a
  second observed transition. A crash between them preserves the approval
  fence. When the driver proves the command could not start, a further
  transition restores the complete prior observed document; failures after a
  possible effect retain the fence. ObservedUnit records this marker and the exact native scope accepted
  by a reasoned missing-state recovery.
- **Afterwards.** The unit's receipt is marked `operator-modified`. While held,
  consumers keep resolving from it, as for any hold. After `resume`, the next
  execution of the unit needs explicit approval, whatever its `approval`
  setting, because the committed source may disagree with what the repair
  left behind, for example after temporary edits were reverted: kinds with
  `change_digest` need `--approve <unit>=<digest>` for the plan that
  `nyl plan --unit` shows against the repaired state, and others a plain
  `--approve <unit>`. Until then the unit is `awaiting-approval`, so a CI job
  exits 2 instead of applying. That execution follows the recovery policy
  (`converge` re-executes, `inspect` inspects, `manual` still waits for
  `recover --retry`), and its receipt clears the mark.
- **Exit status.** Once the tool has started, `exec` exits with the tool's
  status, or 128 plus the signal that ended it. Before that, Nyl refuses with
  exit 125 and a stable code, such as `NYL-EXEC-NOT-HELD`,
  `NYL-EXEC-UNSUPPORTED`, `NYL-EXEC-SCOPE-CONFLICT`, `NYL-LEASE-HELD`, or
  `NYL-LEASE-BROKEN` (a broken lease whose run `nyl recover -e <env>` has not
  imported), so scripts can tell a refusal from the tool's own failure.
- `exec` ignores deployment admission: it repairs a unit, it does not deploy a
  source.

## Deletion

Deletion and holds are recorded in the desired unit's `lifecycle` block, so a
unit's whole history is one file's diff and the last desired spec needed for
teardown is already there. Every lifecycle change goes through one transition
table, `nyl_orchestration::rules::lifecycle::transition`; what a run deletes,
retains, or keeps waiting is decided by `rules::desired` and
`rules::teardown`, and what a hold writes by `rules::hold`:

```yaml
lifecycle:
  state: deleting                    # active | deleting | held | retained
  deletion:
    reason: omission                 # omission | teardown | replace | kind-change | expired | decommission
    intent: teardown                 # retain | teardown
    teardown: pending                # pending | in-progress; absent for intent: retain
    requested: {by: alice, at: 2026-09-24T12:00:00Z, reason: decommission}
```

- `reason` says what requested the deletion: leaving the ownership set
  (`omission`), `nyl teardown --unit` of a unit outside it (`teardown`) or
  inside it (`replace`), a kind change (`kind-change`), an expired template
  instance (`expired`), or `teardown --all` and `state delete --teardown`
  (`decommission`).
- `teardown: pending` means the teardown has not started, because it awaits
  `--allow-teardown`, an approval, a dependent's teardown, or a readiness
  override. `in-progress` means the driver's teardown has started, including a
  publication waiting under its `teardownWait`. A teardown that failed or is
  uncertain stays `in-progress` with its condition beside it: the unit reads
  `tearing-down`, `status` exits 3 or 4, and the next attempt needs no new
  `--allow-teardown`.
- The status values are projections of this block: `pending-teardown` is
  `deleting` with `teardown: pending`, and `tearing-down` is `deleting` with
  `teardown: in-progress`.

A held unit's block looks like this:

```yaml
lifecycle:
  state: held
  hold:
    since: 2026-09-24T12:05:00Z
    by: alice
    reason: "incident 4711"
    pending:                         # newer source not applied while held
      sourceCommit: 5a90…
      executionKey: sha256:…
      deletion: false                # true when the unit left the ownership set
```

- **Omission.** A unit that leaves the ownership set (removed from source, for
  example with `nyl delete unit <name>`, deselected, or `enabled: false`) keeps
  its desired file with `state: deleting`, `reason: omission`, and the intent
  from its `deletionPolicy`. Units still referencing it make resolution fail,
  so a unit and its consumers are removed together.
- **Default policy.** `deletionPolicy` defaults to `Teardown` for kinds that
  support teardown and to `Retain` for kinds that do not, such as `OciImage`
  and a `Command` without a teardown step.
- **Teardown.** The driver's teardown runs from the desired file's spec, with
  the driver's teardown recovery policy. Teardown follows the reverse of
  creation order: a unit is torn down only after every deleting unit that
  depends on it, so consumers go first and producers follow once nothing uses
  them. The same reverse-dependency rule orders every drop and every declared
  retention: a unit leaving with any intent is dropped, retained, or released
  only once its deleting dependents are gone. A `KubernetesPublication`
  counts as torn down only once its `teardownWait` completes, so the units it
  depends on, and transitively theirs, wait for it. Units with no dependency
  path to a waiting unit are torn down in the same run without waiting.
  Teardown caused by omission or a kind change requires `--allow-teardown` on
  `reconcile` to start; without it the unit is `pending-teardown` with
  `NYL-TEARDOWN-PENDING` and the run exits 2, so an unintended omission, such
  as a selector typo, destroys nothing, and fixing it returns the unit to
  `active` with its uid and receipt. `--allow-teardown` gates only the start:
  a plain `reconcile` continues every teardown that started, and every
  teardown an operator requested with `nyl teardown`, unless the unit is
  held. When teardown succeeds, the run removes both files and reports the
  unit `removed`. Lifecycle work runs after the execution waves; `reconcile
  --leaving` does only that work, and `reconcile --unit` none of it.
- **Retained dependents.** A `retained` tombstone keeps running resources that
  may still use its producers. Tearing down a unit that a tombstone's receipt
  cites in its provenance refuses unless the invocation passes
  `--allow-dependents`, as for units other environments reference: the unit
  stays `pending-teardown` with `NYL-TEARDOWN-HAS-DEPENDENTS`, exit 2; `plan`
  lists such tombstones with the teardown, and the transition commit records
  the override.
- **Retain, declared.** A unit that declares `deletionPolicy: Retain` keeps
  its resources when it leaves the ownership set. The run replaces its desired
  and observed files with a `retained` tombstone: the last desired document,
  uid, and receipt, under `lifecycle.state: retained`. A unit that returns with
  the same name and kind adopts the tombstone (`rules::desired::adopt_tombstone`):
  with an unchanged execution key it is current at once, even when a
  producer was recreated in the meantime; otherwise it is resolved and runs
  as an ordinary change of the same incarnation. Restoring an accidentally
  omitted unit therefore never re-runs it while its inputs are unchanged. The
  tombstone keeps the unit's artifact files, so a retained image's digest
  stays readable; its observed file, drift record, and attestations are
  removed. `nyl status` lists tombstones, and the transition commit's summary
  lists every newly retained unit under `details.retained`.
- **Retain, by kind.** A unit whose kind cannot tear down and that does not
  declare `Retain` leaves no tombstone: the run releases it, removing its
  files, reporting it `released` with `NYL-UNIT-RELEASED`, and listing what
  was left in place, such as an image reference, under `details.released` in
  the transition commit. Returning later starts a new incarnation.
- **Forgetting.** `nyl state forget -e <env> --unit <u>` drops a `retained`
  tombstone or a `pending-teardown` unit from state without touching its
  resources, for example to let a Teardown unit go without destroying it.
  A returning unit then starts a new incarnation.
- **Explicit teardown.** `nyl teardown -e <env> --unit <u>` tears a unit down
  whether or not it is in the ownership set. The command is itself explicit
  intent, so it needs no `--allow-teardown`. It runs on the shared run path
  as a run mode, from recorded state only: it renders nothing, checks
  ownership against the recorded ownership set, and runs each teardown with
  the files of the unit's recorded `sourceRevision`. When that commit cannot
  be fetched, the teardown fails before any effect with a fix naming
  `--source <revision>`, which the transition commit records under
  `details.sourceOverride`. Unknown unit names are refused.
  - For a unit outside the ownership set (a `deleting` unit, or a `retained`
    tombstone, whose last desired document it holds), it completes the
    deletion and removes the files, including the tombstone.
  - For a unit still in the ownership set, it replaces the unit
    (`reason: replace`): the incarnation is torn down, and the next
    `reconcile` writes a new desired file with a new uid. Dependents resolve
    against the new receipt's outputs and run only if their execution key
    changes. This rebuilds corrupted resources or rotates something that can
    only be recreated.
  - A unit whose kind cannot tear down is released, as `--all` does: its
    resources stay and its state is removed, so a unit still in source is
    created fresh by the next `reconcile`, unless `--hold` keeps it.
  - Only the named units are touched. A named unit that a deleting unit still
    depends on waits for it: it is reported `pending-teardown` and the command
    exits 2.
  - `--hold` on a unit that is not held means "tear down, then hold": the
    hold rides on the last event of the teardown or release, so no run can
    recreate the unit between the teardown and a separate `nyl hold`, and the
    held file stays without a uid. When the teardown does not finish in the
    invocation, because it waits, failed, or is uncertain, the deleting unit
    is held with its deletion block, so nothing recreates it; after `resume`
    the teardown continues, and the unit is held again once it completes, so
    it is not recreated until it is resumed again.
  - `--all` tears down every unit, honouring `deletionPolicy: Retain` (see
    [Instances](#instances)).
  - `--fail-fast` stops the teardown after the first failure, uncertain
    outcome, or approval wait (see [Execution](#execution)).
  - A unit that needs manual approval is reported `awaiting-approval`, exit 2,
    until the invocation supplies a matching approval.
- **Holds freeze a unit.** `nyl hold -e <env> --unit <u> [--reason <text>]`
  makes runs change nothing about the unit, whatever state it is in, without
  touching its resources. A held unit never progresses. `nyl hold` writes the
  final held document in its own commit and never renders source; later runs
  record what source would change under `hold.pending`, with its source
  commit and execution key, write the held file only when that record
  changes, apply nothing else, and report the unit `held` (exit 2) with
  `NYL-UNIT-HELD` when changes or a teardown are pending:
  - A unit with a running incarnation keeps the desired document its receipt
    was produced for: the hold reverts to the receipt's `desired` copy in the
    observed document (see [Observed unit](#observed-unit)), never to a
    document found in history, and records a newer document under
    `hold.pending`. Its receipt stays current against that document, so
    consumers keep resolving from it; the frozen state is what actually runs.
    This holds for a unit with an `uncertain` condition too: its desired state
    is the document and uid of its last receipt, the condition stays, and
    `resume` does not clear it (see [Recovery](#recovery)). An uncertain unit
    without a receipt keeps its dependents blocked.
  - A unit without an incarnation (after a teardown, or declared but never
    executed) is not created. Its desired file has no uid, and its dependents
    stay blocked. A unit with state loses its uid only when nothing shows it
    ever ran: no receipt, no condition, and no leftover run listing it.
  - A unit that has never been reconciled in the environment can be held too:
    `nyl hold` writes a held marker, a desired file with `lifecycle.state:
    held` and no uid, kind, or rendering (see [Desired unit](#desired-unit)).
    Runs exclude it until `nyl resume`, and its consumers stay blocked.
  - A unit that is `deleting` pauses: no retention or teardown happens. Its
    file has `state: held` and keeps its `deletion` block next to `hold`, so
    `resume` returns it to `deleting` where it stopped.
  - A `retained` tombstone cannot be held, because nothing of it runs:
    `nyl hold` refuses it with `NYL-HOLD-REFUSED`.
  - A held unit that leaves the ownership set is not deleted; it is reported
    as held with a pending deletion. After `nyl resume`, a held file without
    a uid outside the ownership set is removed by the next `reconcile`.
  - `nyl teardown --unit` of a held unit only records the request, as the
    held file's pending deletion: it tears nothing down and exits 0,
    reporting the unit `held` with a pending teardown, listed under
    `details.teardownRequested`. After `nyl resume`, the next
    teardown-capable run, a `reconcile` or a `teardown`, carries it out as an
    explicitly requested teardown. `teardown --all` records the request on
    held units the same way, tears down the rest, and then holds what
    remains in the ownership set.
  - `nyl resume -e <env> --unit <u>` lifts the hold and deletes nothing
    itself. The next `reconcile` renders the unit from source again and
    applies whatever changed, including a pending deletion.
  - Holding a held unit and resuming one that is not held change nothing:
    both exit 0 with a note and write no commit, so scripts can repeat them.
  - `nyl hold` while a reconcile or teardown run holds the environment's
    lease is delivered to that run through its signal ref, as a `Signal`
    with `action: hold`; it reports `NYL-HOLD-DELIVERED` with the run it
    went to and exits 0. The running run reads its signals before it starts
    each unit, starts no held unit, writes the hold in its own transition
    commit (listed under `details.held`), and reports the unit `held`;
    units already running finish first. A unit the run already executed or
    created is held as the incarnation that ran: its uid and the document
    of its new receipt. When the run finished before it read the signal,
    `nyl hold` deletes the signal and applies the hold directly under its
    own lease (owner: `nyl_orchestration::shell::signal`). `nyl hold`
    reports a delivery only while the run still holds the environment
    after the signal is pushed, and the run reads its signals once more
    after its transition commit, once it holds nothing (its lease released
    and its run ref marked finished) and before its run ref and signal ref
    go: a hold it had not read yet is applied as `nyl hold` applies it,
    requested by the signal's sender, under a lease of its own that it
    waits for up to the lease grace period, or delivered to a run that took
    the lease since (owner: `nyl_orchestration::shell::run`). Every hold
    reported delivered therefore takes effect. Only reconcile and teardown
    runs read signals: while any other operation holds the lease, such as
    `detect-drift`, `attest`, `recover`, or a state operation, `nyl hold` meets a held
    lease like every state operation (`NYL-LEASE-HELD`, exit 2, or a wait
    with `--wait-lease`). A broken lease refuses `nyl hold` with
    `NYL-LEASE-BROKEN`, as it refuses every non-importer.
  - A hold is for incidents and investigations: stop reconciling a unit while
    something is wrong, without a source change and its review cycle. It is
    the only desired state that does not come from source, and it is an
    explicit, recorded operator action. Long-term changes belong in source.
- **Source removal and teardown.** `nyl delete unit <name>` edits source like
  the other `nyl delete` resources: it removes the declaration after checking
  that the remaining project is valid, which rejects removing a unit other
  units still reference. The removal takes effect in every environment that
  selected the unit, at the next `reconcile`, under its deletion policy. `nyl
  teardown` acts on state directly and never edits source.
- **Fencing.** A new incarnation cannot take a name whose desired file is
  `deleting`; it waits until the file is removed, then receives a new uid. A
  kind change under the same name is a deletion with `reason: kind-change`,
  reported with `NYL-UNIT-KIND-CHANGE`, whose teardown starts only with
  `--allow-teardown` like an omission; a `retained` tombstone of another kind
  is dropped, with its resources left in place, and the new kind starts a new
  incarnation.

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
initialization, move, or fresh start, each as its own event.

- The first run for an environment requires `nyl state init -e <env>`, which
  writes `state.yaml` at the configured location. `reconcile` never creates
  state implicitly.
- `state init` creates a missing local backing repository as bare, including
  its parent directories, before taking the lease. Automatic creation accepts
  absolute filesystem paths and local `file://` URLs whose read and write
  endpoints name the same path. Existing entries must already be Git
  repositories; initialization never converts an ordinary directory into one.
  Remote repositories and separate read/write destinations must be provisioned
  externally. Other operations never create a backing repository.
- When an Environment's `state` points to a location without `state.yaml`,
  `reconcile` refuses and names both ways forward:
  - `nyl state move -e <env> --from <repository> [--desired-ref …]
    [--observed-ref …] [--state-path …] [--coordination-refs …]` relocates
    the state to the location the Environment configures. The options name
    the old location and each defaults to the configured value; `--from`
    takes a URL, a GitRepository name, or `local`. It holds the lease at both
    locations throughout, pushes the existing refs' history to the new
    location, and then writes a final transition commit to the old location
    whose `state.yaml` records a `state-moved` event naming the new one. Units
    keep their uids and receipts. Every command against the old location then
    refuses and names the new one, including runs from older source commits
    that still point there. The old refs are kept for audit, and removing them
    is a manual step. An interrupted move is completed by running it again:
    it finds the matching history at the new location and writes the marker.
    The new location is usable only once the marker exists.
  - Copying state into another environment is deliberately not offered: a new
    environment owns none of the old one's resources, so its units start as
    new incarnations with `state init`.
  - `nyl state init -e <env> --fresh` starts over deliberately. Every unit
    becomes a new incarnation and executes again: Terraform and OpenTofu
    converge against their existing backends, images are rebuilt, and
    non-idempotent commands run again. The command prints what that means and
    records a `state-initialized` event marked fresh.
  - A move within one repository changes only the refs, for example to
    rename them: after the Environment's `desiredRef` and `observedRef` change,
    `nyl state move -e <env> --desired-ref nyl/<env>/desired --observed-ref
    nyl/<env>/observed` names the old refs. Moving state to full `refs/…`
    names works the same way and gives up branch protection, with the
    `NYL-STATE-UNPROTECTED` warning. Where neither location shares its refs
    through `path`, the refs' history moves; otherwise the documents move with
    one commit, and the old location keeps the history.
  - `state.yaml` also records the coordination prefix. A run takes the lease at
    the configured prefix and, holding it, refuses when `state.yaml` records
    another. After the configured prefix changes, `nyl state move -e <env>
    --coordination-refs <old prefix>` moves it: it holds the lease at both
    prefixes, fails while a run ref exists at the
    old one, and records the new prefix in a transition commit. A run from an
    older source commit then takes the old lease, reads the new prefix, and
    refuses, so no two runners can hold leases at different prefixes.
- A location whose `state.yaml` names a different environment is an error, so
  two environments can never share state by accident.
- A location whose `state.yaml` carries the deleted marker of `nyl state
  delete` (a last event `state-deleted`) refuses every command, a plain
  `nyl state init` included, with `NYL-STATE-DELETED`, exit 1, naming both
  ways forward: removing the Environment from source, or `nyl state init
  --fresh`, which appends a `state-initialized` event and starts over (see
  [Instances](#instances)). The check reads the marker, never commit trailers
  (owner: `rules::state_location::check_record`).

### Local runs

Each state identity has a local Git repository. A configured local path or
`file://` URL opens that repository directly, including bare repositories.
When the source checkout represents the configured state repository, Nyl uses
that checkout. Other identities use a bare repository in the project cache.
State moves transfer the required objects between repositories.

Ordinary commands fetch the configured state, take the remote lease, and
publish state and coordination updates with compare-and-swap. A local view
with unpublished receipts or checkpoints stops an online operation before it
can overwrite or merge them. Inspect and recover the local work before using
remote state again. `state reset --local` follows the remote only after the
operator explicitly accepts unreflected receipts; unpublished coordination
records must first be recovered locally. `state restore --to` writes forward
commits with historical state content and preserves current admission.

Run and read commands with `--local` use the same local state repository without
fetching, pushing, or acquiring a remote lease. A missing cached repository is an error that names
the path and asks for an online state command first. Local effects are real,
and another operator can run concurrently against remote state. Source
selection is independent of the state repository.

### Pinned commits

Every commit Nyl pins must stay fetchable: locked unit sources, promoted source
revisions, and locked `fromGit` bindings.

- A pinned commit must be reachable from one of the Environment's
  `protectedRefs` in its repository, or from the branch named by its lock's
  own `revision`. The default `protectedRefs` is each repository's default
  branch, so a lock to a long-lived deploy branch works without configuration.
  Resolution checks this when a lock or promotion enters desired state;
  `nyl promote` checks it against the target environment.
- A run's source commit S must also be reachable from `protectedRefs`, because
  later teardown and re-execution check it out again. An environment can set
  `allowUnprotectedSource: true`, typical for dev and preview environments, to
  run from any commit, such as a feature branch. Once such a branch is
  deleted, teardown or re-execution of units last executed from it fails until
  they are reconciled from a reachable commit again; `status` warns about
  units in that state.
- An unreachable commit is a resolution error that names the commit and the
  refs searched. A commit from a squash-merged branch is therefore rejected
  before anything depends on it.

### Credentials and branch protection

- State pushes use the same Git credentials as `publish-tree`, through the
  GitRepository's `publishURL`. Remote Git operations run the `git` command
  line, so `publish-tree` authenticates as the user's `git` does (SSH config
  and keys, SSH agent, credential helpers); state pushes use the same
  transport. Runners without those can name dedicated credentials, optional
  and per repository URL prefix, in `[[git.credentials]]` of `nyl.toml`: an
  SSH key file, an SSH key or an HTTPS token from an environment variable.
  `nyl.toml` holds only where a secret is.
- Nyl never force-pushes a state ref. A non-fast-forward state ref is treated
  as corruption and stops every command until an operator repairs it.
- Recommended protection: force pushes and deletion are disabled on both state
  refs. Pushes are limited to the runner identity plus an operator group, since
  `hold`, `resume`, `recover`, and `teardown` may run from an operator's
  machine; the desired ref additionally accepts promotion pull requests with
  required reviews. A team that wants only the runner to push runs operator
  commands through a manually triggered CI workflow instead. The lease ref
  needs the same pushers. Template instances with their own refs also need
  ref deletion for `state delete`, limited to the template's ref pattern;
  instances sharing a ref through `state.path` do not.

### Defaults

| Setting | Default | Configured in |
| --- | --- | --- |
| Concurrency per wave | 4 | `nyl.toml`, `--concurrency` |
| Lease grace period | 10 minutes | `nyl.toml` |
| `resolutionTimeout` | 30 minutes | `nyl.toml` |
| Attestation refresh | 15 minutes | `nyl.toml` |
| `timeout` | Command 30m, Terraform/OpenTofu 60m, OciImage 60m, KubernetesPublication 10m | unit `timeout` |

## Drivers

Shared orchestration selects behavior through driver capabilities and generic
contracts. Drivers own validation, timeout defaults, committed-source
requirements, resource identity normalization, and retry context validation.
The engine never branches on a concrete kind or vendor diagnostic code.

Built-in drivers are Rust implementations behind one trait:

```rust
trait Driver {
    fn api_version(&self) -> &'static str; // units.gitops.nyl/v1 for built-ins
    fn kind(&self) -> &'static str;
    fn behavior_version(&self) -> u32;
    fn default_timeout(&self) -> Duration; // used when the common timeout is omitted
    fn capabilities(&self, spec: &Spec) -> Capabilities;  // a Command tears down only with a `teardown` step
    fn recovery(&self, spec: &Spec) -> RecoveryPolicies;  // for execution and teardown; a Command converges only when idempotent
    fn spec_schema(&self) -> Schema;        // kind fields; common fields are added by Nyl
    fn validate_spec(&self, spec: &Spec) -> Result<()>;  // kind rules beyond the schema
    fn secret_refs(&self, spec: &Spec) -> Result<Vec<String>>; // explicit driver credential provider keys
    fn credential_environment(&self, spec: &Spec, process_env: &EnvironmentSnapshot) -> Environment; // pure admission from the supplied snapshot
    fn attestations(&self, spec: &ResolvedSpec) -> Vec<AttestationName>; // names it can produce for this spec
    fn ownership_scopes(&self, environment: &EnvironmentName, spec: &ResolvedSpec) -> Vec<OwnershipScope>; // see Identity; an image tag prefix names the environment
    fn execution_key_inputs(&self, ctx: &ResolutionContext, spec: &ResolvedSpec) -> Result<KeyInputs>; // selected files, excluded fields, fingerprint, recorded fields

    fn plan(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<PlanReport>>;
    fn reconcile(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Outcome>>;
    fn detect_drift(&self, ctx: &ExecutionContext, unit: &DesiredUnit, receipt: &Receipt) -> Result<Supported<DriftDetection>>; // `nyl detect-drift`
    fn attest(&self, ctx: &ExecutionContext, unit: &DesiredUnit, receipt: &Receipt) -> Result<Supported<AttestationCheck>>; // `nyl attest`; default Unsupported
    fn observe(&self, ctx: &ExecutionContext, unit: &DesiredUnit, receipt: &Receipt) -> Result<Supported<ObservedValues>>; // `nyl observe` and `reconcile`; see Observed values
    fn inspect(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Inspection>>;
    fn teardown(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Outcome>>; // lifecycle: deleting
    fn teardown_readiness(&self, unit: &DesiredUnit) -> Result<Supported<TeardownReadiness>>; // readiness from the stored document; default Unsupported
    fn teardown_wait(&self, spec: &ResolvedSpec) -> WaitStrategy; // `observe`, `delay`, or `manual` from the stored spec, with the kind's defaults; default manual
    fn observe_removal(&self, ctx: &RemovalContext, unit: &DesiredUnit) -> Result<Supported<RemovalObservation>>; // the `observe` teardown wait: what of phase 1's removals remains; default Unsupported
    fn build(&self, ctx: &ExecutionContext, unit: &DesiredUnit, push: bool) -> Result<Supported<Vec<Artifact>>>; // outside orchestration
    fn exec(&self, ctx: &ExecutionContext, unit: &DesiredUnit, args: &[OsString]) -> Result<Supported<ExecStatus>>; // operator repair, see Operator exec
}

struct Capabilities {
    plan: bool, reconcile: bool, detect_drift: bool, inspect: bool, teardown: bool,
    attest: bool,               // `nyl attest` asks the driver, such as for Argo CD acceptance and health
    observed_values: bool,      // `observe` reads declared observed values; validate_spec checks each `from`
    build: bool,               // supports `nyl build` with driver-owned arguments
    change_digest: bool,        // plan reports a digest, so `bind: plan` is allowed
    exec: Option<ExecTerminal>, // None: no `nyl exec`; Some(Interactive | NonInteractive)
}

enum Supported<T> {
    Supported(T),
    Unsupported,
}

enum Outcome {
    Succeeded { outputs: Outputs, artifacts: Vec<Artifact>, attestations: Vec<AttestationResult> },
    AwaitingApproval { digest: ChangeDigest },               // plan changed since approval; no effects
    Waiting { phase: String, reason: String, until: Option<Timestamp>, published: Option<CommitId> }, // effects so far are recorded; call again
    Failed { category: FailureCategory, retryable: bool, message: String },
    Uncertain { reason: String },
}
```

| Type | Content |
| --- | --- |
| `PlanReport` | `changes`: `none`, `create`, `update`, or `destroy`; a human `summary` with secrets masked; `changeDigest` when the capability reports one; `blockedBy` pointers the driver could not plan without |
| `Receipt` | What an `ObservedUnit`'s `receipt` holds (see [Observed unit](#observed-unit)); drivers receive it and never write it |
| `Outputs` | A JSON object of declared, non-sensitive outputs; Nyl validates it against the declarations and drops the rest before recording |
| `Artifact` | An artifact document without `metadata.unit` and `executionKey`, which Nyl fills in |
| `DriftDetection` | `drift`: `clean`, `drifted` with a summary, or `unknown` with a reason; driver-specific `details`, recorded unchanged |
| `AttestationCheck` | `Attested { outcomes }`, or `Unknown { reason }` when the driver could not tell, such as an attest step that failed; an unknown check records nothing |
| `AttestationOutcome` | `name`, `result` (`pass` or `fail`), an optional `reason`, and driver-specific `details` recorded unchanged, such as each Application's running publication and health |
| `ObservedValues` | Per declared observation: `Value(value)`, `Pending { reason }`, or `Failed { reason }`; Nyl checks values against the declared types, records them, and shows reasons unchanged (see [Observed values](#observed-values)) |
| `Inspection` | `Applied { outputs, artifacts }`, `NotApplied`, or `Unknown { reason }`, the three results of the `inspect` recovery policy |
| `KeyInputs` | What resolution needs to compute an execution key: the selected `files` with their digests, the `excludedFields` of the resolved spec, an optional `fingerprint`, and `recorded` facts. Each recorded fact is a JSON Pointer to a member of the resolved spec and the value resolution writes there before it computes the key, so the stored desired document carries what the driver learned from its inputs. The engine stores a recorded value without interpreting it |
| `TeardownReadiness` | What the driver found about whether the unit's teardown can complete: `blocking` requirements and `remnants` a teardown leaves, each a finding with a stable code and the setting that fixes it, and `not_checked` requirements the driver cannot see. The driver evaluates its own requirements (the publication driver from the settings it recorded at resolution: whether the catalog prunes, the Application deletion policy, each owned Namespace's delete policy, and whether a parent owns the catalog); the shell only displays the findings, keeps a unit with a blocking one pending teardown, and warns in `plan` and `reconcile` |
| `WaitStrategy` | How a teardown waits after phase 1: `Observe`, `Delay { duration }`, or `Manual`. The driver reads it from the stored spec with its defaults (the publication driver: `teardownWait`, else `observe` for a unit that declares `observe` and `manual` for one that does not); `rules::teardown_wait` decides what each strategy does, and the shell reads no spec field itself |
| `RemovalObservation` | What `observe_removal` found: `Remaining(names)` of what phase 1 removes that still exists (empty when all is gone), or `Failed(reason)` when the driver could not read it, shown unchanged as `observation-failed`; its `RemovalContext` carries the unit's admitted credentials (shown by name only in `Debug`) and the wait's deadline, and no checkout, because a unit is torn down from its stored document |
| `IncarnationDirectory` | `ExecutionContext.incarnations`: whether a named unit incarnation (environment, unit, uid) still exists in its environment's state, as `exists`, `gone`, or `unknown` with a reason. A driver that fences a native resource by owner asks it about the owner it finds recorded; the shell answers from the declared environments' state without knowing what is fenced |
| `WaitCompletion` | How a teardown wait ended: `observed`, `delayed`, `confirmed` (with `by`, `reason`, `from`), or `incomplete`; the run decides it and the transition commit records it |
| `ExecStatus` | `Exited { code }` or `Signaled { signal }` of the operator's tool; `exec` reports how the tool ended, never an outcome or receipt |
| `OwnershipScope` | A driver-defined namespace, canonical key components, normalized path segments, and display description. Equal namespace/key claims overlap when either path contains the other by segment. |

- `Waiting` is how a two-phase teardown reports that phase 1 is published and
  its `teardownWait` has not completed: the unit stays `tearing-down`, the
  run records the wait in the observed document and checkpoints it, and the
  driver is called again in the same run while the wait allows, or in a later
  run. Only teardown may return it. `published` is the phase 1 commit, and
  `until` is advisory: the run computes the wait's end from the recorded
  `since` and the strategy. Once the wait ends, the run calls `teardown`
  again with `ExecutionContext.teardown_wait` set to the `WaitCompletion`,
  which tells the driver to run phase 2; every other call passes none.
- `teardown_readiness` reads the stored desired document only, so `status`,
  `get units -o wide`, `plan`, and a teardown show the same readiness without
  rendering. A held marker has no kind and never reaches a driver.
- `Result` errors are Nyl-side problems, such as an unreadable worktree; they
  give the unit an `uncertain` condition when an effect may have started, and
  otherwise fail the invocation with exit 1.
- `ExecutionContext` provides the worktree at the effective source revision
  (for `exec --worktree`, the snapshot described in
  [Operator exec](#operator-exec)),
  the resolved spec, admitted environment variables and secrets, the approved
  change digest for `bind: plan` units, the deadline, a cancellation signal,
  the unit's observed file, the incarnation directory, and a transcript sink
  that masks secret values.
- Every execution gets its own worktree and its own temporary directory for
  tool state, such as `TF_DATA_DIR`, so parallel units sharing a `source.path`
  never share a working directory.
- The **effective source revision** of a Git source field is the commit
  resolution wrote into it (`resolvedSpec.source.commit` and its
  equivalents): S, a locked `commit`, a resolved `revision`, or a promoted
  revision. Its worktree is a detached checkout of that commit.
- Drivers run external tools only through the context, so transcripts,
  masking, and deadlines are uniform. Each tool runs in its own process group.
  At the deadline, or on cancellation, the context sends SIGTERM to the group,
  waits up to 30 seconds (less when the run itself is being cancelled, see
  [Runs and leases](#runs-and-leases)), and then sends SIGKILL. A reconcile or
  teardown ended this way, or by the tool dying from a signal, is `uncertain`,
  because effects may have happened; a plan, drift detection, or attestation ended this way
  fails with category `timeout`.
- Transcripts are masked as they are written and printed on stderr unless
  explicitly suppressed. Optional local log files contain the same masked public
  lines, never private stdout. State records no transcript.
- Methods for capabilities a driver lacks return `Supported::Unsupported`, which is
  reported, never silently skipped.

### Direct builds

A kind whose only effect is producing an artifact from source declares the
`build` capability, and `nyl build <unit> [-e <env>] [-- <driver-args>...]` runs it outside
orchestration:

- It calls the driver's `build` method, without a lease, a receipt, or
  any state write, and prints the artifact document.
- Files come from the worktree it runs in, uncommitted changes included, like
  `nyl render`; `--source <rev>` builds at a commit instead. Nothing is
  recorded, so a dirty worktree cannot corrupt state.
- With `-e`, templates see that environment's `values`, and references
  resolve read-only against its current receipts, exactly as `nyl get output`
  does. Without `-e`, a spec that uses environment values or references fails
  with an error naming the reference and `-e`; there are no per-kind override
  flags in the generic CLI. Arguments after `--` reach the driver unchanged;
  each driver defines and validates its own build options.
- A direct build prints the documents it produced: the artifacts of its kind,
  and for a driver that has a read-only mode, such as the publication
  driver's `--diff`, a report document in the same shape. Nothing records
  them, and the exit is 0 whatever such a report finds.
- The capability is a boolean declaring support for direct builds. The driver
  owns publishing policy and validates its arguments. For example, the image
  driver accepts `-- --push`; a kind that cannot safely publish outside
  orchestration rejects such a request and names `reconcile` instead.
- Kinds with external effects never declare `build`: a Terraform apply or a
  command without a receipt would leave state describing something else. Their
  direct forms are `nyl plan --unit` and `--local` runs.
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
- The plugin implements `plan`, `reconcile`, `detect_drift`, `attest`,
  `inspect`, and `teardown` over the same serialized types. Nyl keeps resolution, leases,
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
  files: ['../scripts/seed/**']
  values:
    bucket: {fromUnit: {unit: storage, output: bucket}}
  command: ['./run.sh']
  detectDrift: ['./drift.sh']  # optional; for `nyl detect-drift`
  attest: ['./smoke.sh']       # optional; for `nyl attest`
  teardown: ['./remove.sh']  # optional; without it the kind cannot tear down
  fingerprint: ['./tool-versions.sh']
  workingDir: ../scripts/seed            # optional; default the nyl.toml directory
  env:
    passthrough: [AWS_REGION, AWS_PROFILE]
    secrets:
      DB_PASSWORD: database-password      # key in the project's secrets provider
  idempotent: true
  outputs:
    seedVersion: {type: string}
  attestations: [{name: smoke}]
  timeout: 10m
```

- `command`, `detectDrift`, `attest`, `teardown`, and `fingerprint` are argument vectors,
  executed directly without a shell; a relative first element is resolved
  against the working directory. `workingDir` follows the local path rule of
  [Units](#units), where `/` alone names the worktree root, and defaults to
  the directory containing `nyl.toml`, in the worktree at S. Commands use S;
  a Command has no `source` field of its own.
- The command starts from an empty environment plus:
  - `PATH` from the runner and `HOME` set to a fresh temporary directory, the
    kind's fixed variables;
  - `NYL_INPUTS`: path to a JSON file holding `{"values": …}` with the
    resolved `values`;
  - `NYL_OUTPUTS`: path to an existing empty file, where the command writes
    one JSON object of outputs;
  - `NYL_ATTESTATIONS`: path to an existing empty file for attestation
    results, when the unit declares attestations it produces, except for
    `detectDrift` and `fingerprint`;
  - `NYL_UNIT`, `NYL_ENVIRONMENT`, and `NYL_OPERATION` (`reconcile`,
    `detect-drift`, `attest`, `teardown`, or `fingerprint`);
  - variables and secrets admitted through the common `env` field (see
    [Credentials](#credentials)).
- Every step has the unit's `timeout` and the termination rules of
  [Drivers](#drivers): a `command` or `teardown` that passes it, or dies from
  a signal, is uncertain; a `detectDrift`, `attest`, or `fingerprint` that
  passes it fails.
- Following the general rule, the execution key covers `command`,
  `detectDrift`, `attest`, `teardown`, `fingerprint`, `workingDir`, `idempotent`, `outputs`, the files matched by `files`,
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
- Exit code 0 with a valid outputs file is success. An empty outputs file
  counts as `{}`, which is valid only when the unit declares no outputs. The
  file may hold at most 1 MiB. Any other exit code is a `tool-error` failure;
  an invalid outputs file, or a declared output that is missing or has the
  wrong type, is an `output-rejected` failure. Undeclared outputs are ignored
  with a warning. Failures are not retryable unless `idempotent: true`, and
  an uncertain execution is re-run only with `idempotent: true` (recovery
  policy `converge`); otherwise it waits for `recover` (`manual`).
  Idempotency for identical inputs is the author's claim, which Nyl cannot
  check.
- `detectDrift` exits 0 for clean, 2 for drift, and anything else for an
  unknown result. `nyl detect-drift` records a drift record, never a
  receipt; the step receives the same inputs and admitted environment as
  `command`, with `NYL_OUTPUTS` and `NYL_ATTESTATIONS` absent.
  Under the default `driftRepair`, drift makes an idempotent command execute
  again in the next reconcile (see [Drift](#drift)).
- `teardown`, when present, gives the kind the `teardown` capability and the
  default `deletionPolicy: Teardown`. `NYL_INPUTS` then holds the resolved
  `values` of the stored desired document under `values` and the last
  receipt's outputs under `outputs`; exit 0
  completes the deletion, and any other exit is a `tool-error` failure with the
  same retry rule as `command`. Without `teardown`, the kind defaults to
  `Retain` and rejects `deletionPolicy: Teardown`.
- A command unit may declare attestations it produces itself, such as
  `attestations: [{name: smoke}]` for a smoke test. The `command` step, or the
  `attest` step that `nyl attest` runs, writes their results to
  `NYL_ATTESTATIONS` as one JSON object of `pass` or `fail` per name; a
  declared name it leaves out stays pending, and undeclared names are
  ignored. `attest` gives the kind the `attest` capability and requires an
  attestation without `from: external`. An `attest` step that does not exit
  0 could not tell: the unit is `unknown` (`NYL-ATTEST-UNKNOWN`, exit 2) and
  nothing is recorded, as `detectDrift` reports `unknown` for an exit other
  than 0 or 2. Driver errors (exit 1) stay Nyl-side problems, such as a
  checkout that failed.
- There is no sandbox beyond the working directory and the environment; the
  command runs with the runner's permissions. Isolation such as containers is
  an M7 decision.
- Secrets require a configured secrets provider; the project must provide one
  before command units can use `env.secrets`.

## Commands

| Command | Effect | Writes |
| --- | --- | --- |
| `plan` | Resolve and run driver planning for ready units and for `deleting` units with teardown intent (destroy plans); report units leaving the ownership set, blocked units, and incomplete plans; `--teardown` previews a requested teardown (see [Planning](#planning)) | Nothing |
| `reconcile` | Resolve, then execute waves until nothing is ready, and carry out lifecycle work: teardowns (`--allow-teardown` to start one caused by omission), retentions, releases, and drops; `--leaving` does only the lifecycle work; `--unit … --reapply --reason …` forces named units; `--fail-fast` stops after the first failure, uncertain outcome, or approval wait | One transition commit per state ref |
| `status` | Report each unit's state from one snapshot of both refs | Nothing |
| `detect-drift` | Ask each driver whether a current receipt still holds (see [Drift](#drift)) | One observed commit with the changed drift records |
| `attest` | Ask the drivers that attest for their units' specs to attest current receipts, or record an external attestation for a unit's current receipt or the environment's current state (see [Attestations](#attestations)) | One observed commit with the changed attestations |
| `observe` | Read and record the observed values of current receipts (see [Observed values](#observed-values)) | One observed commit with the changed observation records |
| `recover` | Import a broken lease's run (`-e` alone), clear an uncertain condition or a failure for re-execution, or with `--accept <run>` adopt a checkpointed result; refuses a unit without a condition, and refuses without `--unit` when there is nothing to import | One observed commit (with a desired commit beside it when needed), the consumed run refs, and deletion of evidence refs no uncertain unit needs; never a run record |
| `teardown` | Tear down a unit, replacing it if still selected; `--hold` keeps it down; `--all` tears down every unit, honouring `deletionPolicy: Retain`; `--confirm-removed` ends a publication's teardown wait on the operator's word, under any strategy; a held unit only records the request; `--fail-fast` as for `reconcile` | One transition commit per state ref |
| `hold` | Freeze a unit, or hold a unit never reconciled with a held marker: reconcile no changes to it until resumed; delivered to a running run through its signal ref | One desired commit, or a signal to the running run |
| `exec` | Run a held unit's tool, through its driver's `exec` capability, with the deployed spec or one rendered from the worktree (see [Operator exec](#operator-exec)) | One observed commit |
| `ci admit` (M8) | Record a new deployment generation, or confirm the current one (see [Deployment admission](ci-execution.md#deployment-admission)) | One commit on the admission ref |
| `ci export` (M8) | Export the dependency graph as a provider workflow (see [CI execution](ci-execution.md)) | Nothing |
| `resume` | Lift a hold | One desired commit |
| `state init` | Create state at the configured location; `--fresh` starts over deliberately; `--template` creates or updates an instance | `state.yaml` on each ref |
| `state delete` | Remove an environment's state after all its units are torn down, for template instances and declared environments; `--teardown` tears them down first | One commit removing the environment's files and leaving the deleted marker in `state.yaml`; the refs and their history stay |
| `state move` | Relocate state from another location and retire the old one | Moved history, plus a `state-moved` commit at the old location |
| `state forget` | Drop a `retained` tombstone or a `pending-teardown` unit without touching resources | One desired and one observed commit |
| `lease break` | Declare a lease holder gone so the next run takes over at once | The lease ref; recorded in the transition commit that imports the broken run |
| `build` | Run a unit's `build` capability from the current worktree, outside orchestration (see [Direct builds](#direct-builds)) | Nothing in state; the artifact is published only through driver arguments (for example `-- --push`) where the kind allows it |
| `promote` | Record a proven source state's values in the target environment (see the [promotion contract](promotion.md)) | One desired commit with the PromotionRecord, or a pull request under `changeGate: pullRequest` |

All are top-level `nyl` commands. `release` is taken by Kubernetes release
history and `delete` by source editing, so lifting a hold is `resume` and
tearing down is `teardown`.

### Multi-environment runs

`plan`, `reconcile`, `status`, `detect-drift`, `attest`, and `observe` accept several environments in one
invocation, in any one of three forms:

| Form | Selects |
| --- | --- |
| `-e a -e b` | The named environments |
| `-l stage=production` | Declared Environments whose labels match the selector |
| `-g production` | The members of an EnvironmentGroup |

- A multi-environment run is a sequence of ordinary single-environment runs,
  one per environment, in one invocation. Each takes its own lease, resolves
  its own source, and writes its own transition commit; nothing is committed
  across environments atomically, and running the environments one by one
  reaches the same state.
- Environments run in dependency order: producers before the environments that
  reference them (see [Cross-environment references](#cross-environment-references)),
  ties by name. A consumer reads its producers' state after their runs, so one
  invocation can converge a chain that separate calls would converge over
  several runs. References to environments outside the selection are read as
  they are.
- Every environment runs, whatever happened to the ones before it. A consumer
  of a producer that failed or is waiting has references that block, exit 2,
  while its independent units still reconcile.
- An environment whose lease is held blocks for `--wait-lease` like a
  single-environment run and is otherwise reported as `busy` and exit 2,
  because the invocation asked for it explicitly; unlike the preview fleet, it
  is not left out of the exit category.
- `plan` plans each environment against current state. A consumer's plan sees
  its producers' current receipts, not what their plans would produce, so a
  unit that reads an output the producer's plan changes is shown as waiting
  for that producer, naming the producer unit and its planned change.
- Unit-scoped options qualify units with their environment: `--unit <env>/<u>`
  and `--approve <env>/<unit>=<digest>`; an unqualified unit name is rejected.
  `--approve-all`, `--allow-teardown`, and `--allow-dependents` apply to every
  selected environment. `--source`, `--template`, and `--param` stay
  single-environment options.
- `teardown`, `hold`, `resume`, `recover`, `promote`, `lease break`, and the
  `state` commands take exactly one environment, because each is a deliberate
  act on one environment's state.
- The typed report has one section per environment in run order, and the exit
  category is the most severe across them under the usual precedence (see
  [Inspection](#inspection)). `--output json` lists the environments with
  their own categories.

### Planning

`nyl plan` is the review tool: a pull request pipeline runs it against the
pull request's head and each affected environment's current state, so
reviewers see what a merge would do before anything runs. It reads state and
never takes the lease.

- It reports every unit that would execute: first creations, updates with the
  driver's change summary, `drifted` units the next reconcile would repair or
  that await approval or a forced run, and units waiting for evidence or
  approval. Held units are reported `held` with their pending change.
- It reports every unit that would leave the ownership set, grouped by what
  would happen, in a section that always comes first:

  ```text
  Units leaving the ownership set of dev (5):
    teardown, needs --allow-teardown:   network, database, kubernetes
    dropped from state, resources kept: web-image, seed (re-created as new incarnations if they return)
    retained as tombstones:             (none)
  ```

  Each leaving unit carries the reason (removed from source, deselected by
  `unitSelector`, `enabled: false`), and a deselection names the labels that no
  longer match, so a selector typo reads as one. The warning code is
  `NYL-PLAN-UNITS-LEAVING`, and `--output json` lists the units under
  `leaving` with their reason and consequence.
- `--fail-on-leaving` exits 1 when any unit would leave, for pipelines that
  want removals to need an explicit override, such as a pull request label.
- `nyl plan --teardown -e <env> (--unit <u> | --all)` previews a teardown that
  has not been requested yet: the destroy plans and digests of what
  `nyl teardown` would remove, so manual units can be approved with
  `nyl teardown … --approve <unit>=<digest>`. Without `--teardown`, `plan`
  shows destroy plans only for units already `deleting`. Leaving units carry
  their teardown readiness and `NYL-TEARDOWN-*` warnings, and `plan` lists
  the retained tombstones a teardown would pass with `--allow-dependents`.

### Inspection

`nyl get` inspects both declarations and state. Without `-e`, it reads source;
with `-e <env>`, it reads that environment's state.

```bash
nyl get units                      # declarations: kind, labels, environments selecting each
nyl get units -e dev               # state: joins desired and observed
NAME        KIND                   STATE              RECEIPT   LAST RUN
network     OpenTofu               current            current   0b8f… 10:07
database    OpenTofu               awaiting-approval  stale     0b8f… 10:07   manual approval (bind: plan)
web-image   OciImage               failed             stale     0b8f… 10:05   tool-error (retryable)
kubernetes  KubernetesPublication  held               current   0a1d… 09:12   pending changes from 5a90…

nyl get unit database -e dev -o yaml             # exact DesiredUnit and ObservedUnit documents
nyl get unit database -e dev -o yaml --observed  # only one of them (--desired, --observed)

nyl get output network/vpcId -e dev              # resolves like {fromUnit: {unit: network, output: vpcId}}
nyl get output database/connection -e dev --pointer /host   # pointer inside an object output
nyl get observation kubernetes/ingressIp -e dev  # resolves like {fromUnit: {unit: kubernetes, observation: ingressIp}}

nyl get artifacts -e dev
UNIT        NAME   KIND            SUMMARY
web-image   image  ContainerImage  registry.example.com/web@sha256:4f0c…
kubernetes  tree   PublishedTree   deploy/dev@9c1e…

nyl get artifact web-image/image -e dev -o yaml
nyl get artifact web-image/image -e dev --pointer /reference   # pointer relative to spec, as in fromUnit

nyl get environments
nyl get promotion-paths
nyl get promotions -e staging      # PromotionRecords with each value's source and evidence
```

- Tables join desired and observed state for reading. `-o yaml` and `-o json`
  return the exact persisted documents, never a synthesized merge.
- `nyl get output <unit>/<output>`, `nyl get observation <unit>/<name>`, and
  `nyl get artifact <unit>/<name> --pointer …` return single values for scripts and resolve exactly like the
  matching `fromUnit` reference: the same pointer base (inside the artifact's
  `spec`), the same digest check, and the same freshness rule. When the
  reference would not resolve (no current receipt, or a missing output,
  artifact, or pointer target), they print nothing and exit 1. `--allow-stale`
  prints the latest receipt's value with a warning instead.
- `--state-revision <commit>` reads state as of an earlier state commit, the
  same name `nyl promote` uses for a state commit; `--local`
  reads the configured local state repository. Output formats: `table` (default), `wide`,
  `yaml`, `json`, `name`.
- `get` only reads: it needs read access to the state repository and never
  takes the lease. Document and table forms exit 0 whenever they can read the
  requested state; value forms follow the rule above.
- `nyl create` and `nyl delete` stay source-only, for environments,
  environment groups and templates, units, and promotion paths. Artifacts and state records have no `create` or `delete`:
  drivers produce them, and state changes only through the operations above.
- `nyl status -e <env>` remains the environment overview: the run holding the
  lease and what it is executing, blockers, and suggested next actions. It
  keeps the exit categories below, so CI can check whether everything is
  reconciled.

Common options: `-e`/`--environment`, `--unit`/`--units`,
`--approve <unit>[=<digest>]`, `--approve-all`, `--approved-by`, `--approval-source`,
`--allow-teardown`, `--allow-dependents`, `--allow-incomplete`, `--confirm-removed`, `--wait-lease`, `--make-room`, `--renew`, `--no-extend`, `--template`, `--source`, `--local`,
`--leaving`, `--generation`,
`--concurrency`, and `--output json` for versioned machine results on
stdout, with human diagnostics on stderr.

Each unit has one state in `status`: `blocked` (with the unresolved reference or
non-current dependency), `ready`, `drifted` (a current receipt whose latest
drift record is `drifted`), `awaiting-approval`, `running` (with the run
holding the lease), `uncertain`, `failed`, `current`, `held`,
`pending-teardown`, `tearing-down`, or `retained` (owner:
`rules::unit_state`). Whether the latest receipt is current is shown as a
separate attribute, and so is a unit's condition next to a lifecycle state:
a held uncertain unit reads `held` with its `uncertain` condition, and a
failed teardown `tearing-down` with its `failed` condition; a condition
decides the exit first (4 for uncertain, 3 for failed). Held markers are
listed as held without an incarnation, tombstones as `retained`, and
`status` and `get units -o wide` show each publication unit's teardown
readiness (`ready` or `incomplete: <reasons>`).

Exit categories, where "selected" means the units the invocation was allowed to
execute (all of them without `--unit`/`--units`):

| Exit | Meaning |
| --- | --- |
| 0 | Every selected unit is current, or its deletion completed; or the run's deployment generation was superseded and it executed nothing |
| 1 | Configuration, resolution, or operational error |
| 2 | Not everything was reconciled, and nothing failed: a unit is held, drifted, blocked by a held dependency, or waiting for evidence, approval, `--allow-teardown`, `--allow-dependents`, readiness, a teardown wait, or another run's lease, or the lease is broken and waits for an import (`NYL-LEASE-BROKEN`) |
| 3 | At least one execution failed, a unit drifted, or an attestation failed |
| 4 | At least one execution is uncertain and needs recovery, the run lost its lease, or the run was cancelled by a signal (`NYL-RUN-CANCELLED`) |

When several categories apply, the most severe wins, in the order 4, 3, 1, 2,
0: an error is never reported as a harmless wait because something else is
also waiting. Holds are reported as exit 2 on purpose:
a run that leaves units unreconciled says so. A pipeline that expects a hold
names the units it should reconcile with `--unit`/`--units`, and then exits 0
when those are current.

The table applies to each command as follows:

| Command | 0 | 1 | 2 | 3 | 4 |
| --- | --- | --- | --- | --- | --- |
| `reconcile`, `teardown` | As the table | As the table | As the table | As the table | As the table |
| `plan` | The plan is complete, whatever changes it shows | Error, or `--fail-on-leaving` with a unit leaving | Incomplete: a unit is blocked, so its change cannot be planned | A driver's planning failed | — |
| `detect-drift` | Every checked unit is clean | Error, including a check that passed its unit's deadline | A unit could not be checked: no current receipt, a kind that cannot detect drift, or `unknown` drift | Drift | — |
| `attest` | Every attested unit's attestations pass | Error, including a check that passed its unit's deadline | A unit could not be attested: no current receipt, `--unit` named a unit whose kind does not attest, or the driver could not tell (`unknown`) | A failing attestation | — |
| `observe` (M5) | Every observation of every observed unit has a value | Error, including a check that passed its unit's deadline | An observation is `pending` or `failed`, a unit has no current receipt, or `--unit` named a unit whose kind cannot observe; a changed value is reported (`NYL-OBSERVATION-CHANGED`) but does not change the exit | — | — |
| `status` | Every unit is current | Error | As the table | A unit has a `failed` condition | A unit is `uncertain`, or a run lost its lease |
| `promote` | See the [promotion contract](promotion.md#exit-categories) | | | | |
| `get`, `validate`, `build` | Success | Error, including a value form that would not resolve | — | `build` only: the build failed | — |
| `exec` | The tool's own exit status once it has started; 125 when Nyl refuses before starting it (see [Operator exec](#operator-exec)) | | | | |
| `hold`, `resume`, `recover`, `attest`, `lease break`, `state …` | Done, including a hold delivered to a running run and a `recover` import that left units uncertain | Error or refusal, including `state delete --teardown` after a failed or uncertain teardown | The lease is held or broken (`NYL-LEASE-BROKEN`), `state init --template` is at `maxInstances`, or `state delete --teardown` waits as `teardown` does | — | — |

`--output json` prints the operation's typed report, the same one that renders
tables and the transition commit summary:

```json
{
  "apiVersion": "gitops.nyl/v1",
  "kind": "PlanReport",
  "environments": [{
    "name": "dev",
    "exit": 2,
    "complete": false,
    "leaving": [{"unit": "seed", "reason": "deselected", "consequence": "dropped", "labels": {"dve": "true"}}],
    "units": {
      "network": {"state": "ready", "changes": "update", "changeDigest": "sha256:…", "summary": "…"},
      "database": {"state": "blocked", "blockedBy": [{"pointer": "/variables/vpc_id", "unit": "network", "reason": "no-current-receipt"}]}
    },
    "warnings": [{"code": "NYL-PLAN-UNITS-LEAVING", "message": "…", "fix": "…"}]
  }]
}
```

Reconcile, teardown, detect-drift, attest, and status reports have the same
shape, with `result` per unit as in the transition commit summary, `state`
in a status report, and `check` in a detect-drift or attest report:
`clean`, `drifted`, `attested` (with the unit's `attestations`), `unknown`,
`no-receipt`, or `unsupported`. The report's
`apiVersion` versions the machine output.

## Walkthroughs

**CI lifecycle (M8).** An MR removes `database` and the binding that consumed
it. Its read-only planning workflow lists `database` as leaving, writes no
state, and fails only when `--fail-on-leaving` is requested. After merge, the
reconciliation export has unit jobs for the remaining units and a lifecycle job
for `database`; without `--allow-teardown` that job leaves it
`pending-teardown`, exit 2, and with it the unit is torn down. Deselecting
every unit still exports a lifecycle node naming all of them.
([`ci-lifecycle`](../nyl/tests/scenarios/walkthroughs/ci-lifecycle/scenario.yaml))

**CI admission and retries (M8).** Deployment A's jobs finish, B is admitted
and deploys, and a worker from A is retried: it reports superseded, executing
and writing nothing. Re-dispatching A is refused as a rollback, while
re-dispatching B returns B. A hold and resume in between leave B current. An
authorized rollback admits C at A's source, which then reconciles. These are
shared core admission scenarios, executed by every CI adapter's conformance
suite. ([`ci-admission`](../nyl/tests/scenarios/walkthroughs/ci-admission/scenario.yaml))

**Separate CI unit jobs (M3 foundation, M8 adapter).** Starting without receipts,
the `database` job is blocked and executes nothing. Separate jobs for `network`
and `web-image` record their results under the normal environment lease. A fresh
`database` invocation resolves `network`'s receipt, then `kubernetes` resolves
both producers. Retrying `database` executes nothing. Every invocation resolves
the full ownership set, and existing competing-run and recovery rules still
apply; these jobs are separate runs, not one distributed transaction.
([`unit-jobs`](../nyl/tests/scenarios/walkthroughs/unit-jobs/scenario.yaml))

Each walkthrough is an executable scenario under
[`nyl/tests/scenarios/walkthroughs/`](../nyl/tests/scenarios/walkthroughs/),
in the format the [reference scenarios](reference-scenarios.md#scenario-files)
define. A change to a rule a walkthrough uses changes its scenario in the same
change. They use one environment, `dev`, with `network` and `database`
(OpenTofu), `web-image` (OciImage), and `kubernetes` (KubernetesPublication,
reading `database`'s `host` and `web-image`'s `image`); until M4 and M5, the
harness runs them with the fake kinds that stand in for those kinds.

**Successful dependency wave.** `nyl plan -e dev` resolves `network` and
`web-image`, which have no references, reports `database` and `kubernetes`
blocked on missing receipts, and exits 2 because the plan is incomplete.
`nyl reconcile -e dev` then runs `network` and `web-image` in wave 1 and
checkpoints both receipts on the run ref. Re-resolution at the same source
commit S makes `database` ready; it runs in wave 2 with `vpcId` in its
resolved spec and provenance citing the run. `kubernetes` runs in wave 3. The
run ends with one desired and one observed commit whose summary lists all four
units as `executed`, and exits 0. A second `reconcile` executes nothing,
writes no transition commit, and exits 0.
([`dependency-wave`](../nyl/tests/scenarios/walkthroughs/dependency-wave/scenario.yaml))

**Unavailable upstream output.** `web-image` fails in wave 1 with a retryable
`tool-error`. `database` is unaffected and runs in wave 2. `kubernetes` stays
`blocked` on `web-image`: its desired document records its rendering at this
run's S with `readiness: blocked` and the unresolved pointer, and nothing
executes it. The transition commit records `web-image`'s failure as its
condition and `kubernetes` as blocked; the run exits 3, because a failure
outranks a wait. A `plan` meanwhile reports `kubernetes` blocked and exits 2.
The next run renders every unit at its own S, which replaces the blocked
document, retries `web-image` because its failure is retryable, and, once the
new receipt is checkpointed, re-resolves `kubernetes` in a later wave of the
same run, which then executes it. It exits 0. Preview values never become
inputs: `kubernetes` executes only against a current receipt.
([`unavailable-upstream-output`](../nyl/tests/scenarios/walkthroughs/unavailable-upstream-output/scenario.yaml))

**Effects without a receipt.** A runner has checkpointed `network`, applies
OpenTofu for `database`, and dies before checkpointing it. Its lease and run
ref keep their deadline, `database`'s 60-minute timeout plus the 10-minute
grace, so a run started before then exits 2 and names the holder. The first
run after the deadline takes the expired lease over, imports the dead run's
run ref, where the checkpoints of `network` and `web-image` match the new
resolution's execution keys and become their receipts, and gives `database`
an `uncertain` condition,
because the dead run was executing it without a checkpoint. OpenTofu's
recovery policy is `converge`, so the same run executes `database` again: the
plan finds no changes, `apply` is skipped, and the receipt is recorded. The
run then executes `kubernetes`. Its transition commit lists `network` and
`web-image` under `imported` and `database` under `recovered`, and it exits 0. Nothing is
assumed from the missing checkpoint. No unit is left `uncertain`, so the run
deletes the dead run's run ref instead of retiring it to an evidence ref.
([`effects-without-receipt`](../nyl/tests/scenarios/walkthroughs/effects-without-receipt/scenario.yaml))

**Competing runners.** Two CI jobs reconcile `dev` at once. One creates the
lease ref with compare-and-swap; the other finds it held, reports the holder's
run ID, runner, and operation, writes nothing, and exits 2. With
`--wait-lease 30m` it waits instead, and runs once the lease is released,
finding nothing left to execute. Deleting the lease ref while the first run
executes does not let a second run start, because the first run's run ref
still carries an unexpired deadline; the first run exits 4 at its next lease
update, marks its run ref finished, and pushes nothing to the state refs. The
next run imports its checkpoint of `web-image` as the receipt, executes
`kubernetes`, and deletes the run ref after its commit. If the holder is
lost, its lease expires and a later run takes over as above. When a runner
dies after `web-image`'s effect and before its checkpoint, an operator breaks
its lease with `nyl lease break` instead of waiting for the deadline. `detect-drift`
then refuses with `NYL-LEASE-BROKEN`, exit 2, naming `nyl recover -e dev`,
because only an importer takes over a broken lease whose run is not imported.
`nyl recover -e dev` imports the dead run without executing: one desired and
one observed commit list `web-image` under `recovered` and the break under
`leaseBroken`, the run ref is consumed, and no run record of its own is
written; it exits 0. The next `reconcile` starts at once, converges
`web-image` under its recovery policy, and executes `kubernetes`.
([`competing-runners`](../nyl/tests/scenarios/walkthroughs/competing-runners/scenario.yaml))

**Promotion with stale source evidence.** Dev's run R1 at source commit S1
recorded receipts for every unit. A later run R2 at S2 changed `web-image`,
whose build failed, so the state R2 wrote has a new `web-image` desired
document without a receipt for its execution key, and `kubernetes` blocked.
Prod's source is `fromPromotion`, so before the first promotion
`reconcile -e prod` exits 2 and names `nyl promote`.
`nyl promote dev-to-prod`, with `evidence: published`, considers dev's
recorded states, the transition commit pairs, newest first. The state R2 wrote
fails: `web-image`, whose artifact the path selects, has no receipt for its
new execution key, and `kubernetes`, which prod also selects, is blocked
without a current receipt, so the whole-source rule rejects it. The state R1
wrote passes. The PromotionRecord
therefore carries S1 and R1's image digest, names R1's desired and observed
commits as `from`, and the command exits 0; `reconcile -e prod` then
publishes prod with R1's image, exit 0. `nyl get promotion-candidates
dev-to-prod` shows R2's state as not reaching `published` and names
`web-image` and `kubernetes`. With `evidence: attested`, a state must also
have every required attestation passing for it, such as dev's
environment-wide `qa`; when no recorded state reaches the level, `nyl
promote` writes nothing, names what lacks evidence in the newest state, and
exits 2 with `NYL-PROMOTE-NO-EVIDENCE`. The walkthrough runs from M6.
([`stale-promotion-evidence`](../nyl/tests/scenarios/walkthroughs/stale-promotion-evidence/scenario.yaml))

**Deletion.** `cache` and its only consumer `worker` are removed from source;
`cache` has a teardown step and therefore the default `Teardown` policy,
`worker` declares `Retain`. The next run replaces `worker`'s files with a
`retained` tombstone, marks `cache` `deleting` with `teardown: pending`,
reports it as `pending-teardown`, and exits 2; `plan` lists both as leaving.
Restoring both before any teardown returns them unchanged and executes
nothing, exit 0: `cache` returns to `active` with its uid and receipt, and
`worker` adopts its tombstone. After a second removal, `reconcile
--allow-teardown` still leaves `cache` pending, exit 2 with
`NYL-TEARDOWN-HAS-DEPENDENTS`, because `worker`'s tombstone cites it;
`--allow-teardown --allow-dependents` tears it down, removes its files, and
exits 0. Restoring both afterwards starts a new `cache` incarnation with a new
uid and the same endpoint, so `worker` adopts its tombstone and stays current:
its execution key is unchanged, and producer identity never makes a receipt
stale; exit 0.
([`deletion`](../nyl/tests/scenarios/walkthroughs/deletion/scenario.yaml))

**Decommissioning.** `nyl teardown --unit cache` of a unit still in source
replaces it: the next `reconcile` creates a new incarnation, and `worker`,
whose execution key is unchanged, stays current. `teardown --unit scratch
--hold` tears down, then holds: `scratch` stays held without a uid, and
`reconcile` does not recreate it until `resume`. A teardown of the held unit
`job` only records the request and exits 0; `resume` returns it to
`deleting`, and the next `reconcile` tears it down. A `Retain` unit leaving
source becomes a tombstone, which `teardown --unit` removes. A kind change
leaves the old incarnation `pending-teardown` with `NYL-UNIT-KIND-CHANGE`.
`state delete` refuses while incarnations remain
(`NYL-STATE-DELETE-INCARNATIONS`); `state delete --teardown` tears every unit
down, releases the kinds without teardown and the declared `Retain` units,
and leaves the deleted marker, so the next `reconcile` and a plain `state
init` fail with `NYL-STATE-DELETED`, while an environment sharing the refs
under another `path` is untouched, and `state init --fresh` starts over.
([`decommission`](../nyl/tests/scenarios/walkthroughs/decommission/scenario.yaml))

**Holds on uncertain units.** A run dies after `producer`'s effect and before
its checkpoint; the next run leaves `producer` `uncertain` and `consumer`
blocked, exit 4. `nyl hold` reverts `producer` to the desired document of its
last receipt, from the copy the observed unit keeps, with the same uid, so the
next `reconcile` runs `consumer` against `producer`'s previous outputs and
reports `producer` `held`, exit 2, while `status` exits 4 for the condition.
`resume` lifts only the hold; `recover --retry` clears the condition, and the
next run executes both. A unit never reconciled is held with a marker without
a uid, which runs exclude until `resume`. A hold sent while a reconcile is
executing is delivered to that run (`NYL-HOLD-DELIVERED`, exit 0), which
starts no held unit and writes the hold in its own commit.
([`held-uncertain`](../nyl/tests/scenarios/walkthroughs/held-uncertain/scenario.yaml))

**Drift repair.** Something outside Nyl changes the resources of an
idempotent Command and of one that is not. `detect-drift` records both as drifted,
exit 3 with `NYL-DRIFT-DETECTED`; `status` shows them `drifted`, exit 2, and
`plan` lists them as units that would execute. The next `reconcile` repairs
the idempotent unit, listed under `driftRepaired`, and reports the other
`awaiting-approval`, exit 2; an explicit approval permits it to run.
Under `driftRepair: manual`, `reconcile` repairs nothing and the unit stays
`drifted` until `reconcile --unit idempotent --reapply --reason …`, recorded
under `reapplied`.
([`drift`](../nyl/tests/scenarios/walkthroughs/drift/scenario.yaml))

**Observed value (M5).** The `kubernetes` publication with `observe: {}`
declares the observed value `ingressIp` from its ingress Service, and the
Command unit `dns` consumes it. The first reconcile publishes; with the
default `observationWait: 0s` the address is not assigned yet, so
`ingressIp` stays `pending` and `dns` is `blocked`, exit 2. The next reconcile
observes the address, records it, and executes `dns`, while `kubernetes`
stays `current`. When the load balancer later changes its address, the next
reconcile records the new value under `observationsChanged` and executes
`dns` again, without executing `kubernetes`. A scheduled `nyl observe` that
finds the next change records it on its own and reports `dns` with
`NYL-OBSERVATION-CHANGED`; `status` then shows `dns` `ready`, exit 2, and the
next reconcile executes it. A reconcile in which the
observer cannot read keeps the recorded value, warns
`NYL-OBSERVATION-FAILED`, writes nothing, and leaves `dns` `current`. After a
change with `observationWait: 10m`, a single run publishes, finds `ingressIp`
`pending` for the new receipt, and polls until the address is observed
again; it is unchanged, so `dns` stays `current`.
([`observed-value`](../nyl/tests/scenarios/walkthroughs/observed-value/scenario.yaml))

**Attested evidence (M5).** `service` declares the attestation `healthy` and
attests it when `nyl attest` runs its `attest` step; `smoke` has
`dependsOn: [{unit: service, attestations: [healthy]}]`, and `dashboard` reads
`service`'s `endpoint` with `evidence: attested`. The first reconcile executes
`service`; a receipt is not an attestation, so `smoke` and `dashboard` are
`blocked` on `service`'s `healthy` (`blockedBy` reason `attestation-pending`,
`NYL-ATTESTATION-PENDING` with its fix), and plan and status show the same,
exit 2. A failing `attest` (exit 3, `NYL-ATTESTATION-FAILED`) leaves them
blocked with `attestation-failing` and `NYL-ATTESTATION-FAILING`. After a
passing `attest`, the next reconcile executes both and not `service`. When
`service` later executes with a new receipt, the attestation of the old one no
longer counts: both are blocked again until `attest` records a pass for the new
receipt, after which they are `current` without executing, because their own
inputs did not change.
([`attested-evidence`](../nyl/tests/scenarios/walkthroughs/attested-evidence/scenario.yaml))

**A publication that waits for health.** `kubernetes` declares
`observe: {waitHealthy: {timeout: 5m}}` and its rollout does not finish in time.
The reconcile executes `network`, `database`, and `web-image`; `kubernetes`
fails, retryable, with category `timeout` and no receipt, because it published and
applied but cannot say it is healthy, and the run exits 3. The next reconcile
executes only `kubernetes`, which converges, records its receipt with the
`accepted` and `healthy` attestations the wait found, and leaves the rest
`current`.
([`health-wait`](../nyl/tests/scenarios/walkthroughs/health-wait/scenario.yaml))

**Preview expiry.** Instance `pr-123` of template `preview` (`ttl: 7d`,
`allowTeardown: true`) was last reconciled with `reconcile -e pr-123` eight days
ago. The next reconcile that reaches it, whether a maintenance job's
`reconcile -e pr-123 --no-extend` or the scheduled `reconcile --template
preview`, finds it expired, runs `teardown --all` (its publication unit is
teardown-ready and uses `teardownWait: {strategy: observe}`, so the run waits
until Argo CD has removed the workloads before removing the rest of the tree
and the units the workloads used), and removes its state
directory; the fleet reconcile then reconciles the other, live instances
without extending them. Had the pull request been updated in the meantime, its
pipeline's `reconcile -e pr-123 --template preview --param pr=123` would have
extended the expiry first.

## Remaining questions

| Question | Needed by |
| --- | --- |
| Built-in approver lookups beyond GitHub, starting with GitLab | After M3 |
| Selecting from unordered status lists in observed values, such as the first load balancer entry with an `ip`, beyond one JSON pointer | After M5 |

Driver-specific questions are in the [infrastructure units contract](infrastructure-units.md).

