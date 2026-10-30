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
rendered GitOps resources. `nyl create` and `nyl delete` gain `environment`,
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
    coordinationRefPrefix: refs/nyl/        # where leases, runs, signals, evidence, and keep refs live
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
  (see [Runs and leases](#runs-and-leases)), and a template instance's keep
  ref, `<prefix><env>/keep`. The default custom
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
- An EnvironmentTemplate takes the same field and may template it from
  `params`, for example `revision: 'refs/pull/{{ params.pr }}/merge'`.
- `--source <rev>` overrides S for one single-environment `plan`,
  `reconcile`, or `teardown`, for example to preview what promoting a commit
  would do to prod, or to tear down after a recorded commit was lost. It never
  changes the environment's `source`: the run records it as that run's S, as
  every run does, and the next run uses the environment's source again. It
  must satisfy the same reachability rules, the transition commit marks it as
  an override, and a fleet reconcile rejects it.
- `reconcile -e <env> --worktree` snapshots Git-visible tracked and untracked
  working files once, with deletions and executable modes. It leaves HEAD and
  the user's index untouched, preserves internal symbolic links without following
  them, rejects traversal through symlinked directories, and excludes ignored untracked
  files. Exactly one named environment is required; `--source`, label selectors,
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
  from a fresh clone. This archives Git-visible uncommitted files in the chosen state repository without source ancestry. `--local` keeps them on this clone's
  state branches instead; it remains independent of source selection. These
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
  | `dependsOn` | list of unit names or `{environment?, unit, evidence?, attestations?}` | `[]` | Ordering without a data reference |
  | `approval` | `auto`, `manual`, or `{mode, bind, requireDigest}` | `auto` | See [Approval](#approval) |
  | `deletionPolicy` | `Teardown` \| `Retain` | `Teardown` where the kind supports teardown, else `Retain` | See [Deletion](#deletion) |
  | `timeout` | duration | per kind (see [Defaults](#defaults)) | Deadline for one execution, teardown, or verification; at most `7d`, since a run holds its lease for it |
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
  declare is a resolution error. A consumer whose producer lacks the evidence
  is `blocked` on it:

  ```yaml
  dependsOn:
    - {unit: kubernetes, attestations: [healthy]}   # smoke tests run against healthy workloads
  ```
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
| `fromPromotion: {value, path}` | A value in a PromotionRecord in this environment's desired state: without `path`, the newest record carrying the value, limited to the source paths when the environment's source is promoted (see [Several paths into one environment](promotion.md#several-paths-into-one-environment)) | The record must exist; broken selectors are errors |
| `fromUnit: {environment, unit, output \| artifact, …}` | An output or artifact of a unit in another, declared environment (see [Cross-environment references](#cross-environment-references)) | As `fromUnit`, against the other environment's current receipt |

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
  consumers only, because they are too short-lived to depend on.
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
  desired state and listing the templates' state locations.

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
  mode: publish        # publish | observe
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
  overrides the name; it must be unique like every target name. Changing the
  name, like any change to the target's identity, replaces the unit through
  teardown.
- **Why both exist.** A DeploymentTarget says what is rendered and where it is
  published; it is complete without orchestration and is what a project
  without environments writes. A KubernetesPublication is the
  environment-independent role an environment executes: one definition, such
  as `kubernetes`, is instantiated per environment and places one target into
  each. Promotion selectors and coverage name that shared unit in every
  environment, templated preview environments need its inline target, and
  deselecting it starts a teardown, which a plain target never implies.
- Generated Argo CD Application and AppProject names must be unique among all
  targets whose Argo CD instance resolves to the same cluster and namespace:
  static targets on explicit or implicit instances, and inline targets alike.
  Validation checks this across every target it can see, including every
  instance of an EnvironmentTemplate, whose inline targets must therefore
  template their names, for example from `environment.name`. M2 extended the
  check from targets sharing an explicit ArgoCDInstance to this rule; inline
  targets and template instances join it with M5.
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
  publication identity. These are new index fields, so orchestrated
  publications write index format version 3 with a migration from version 2;
  trees published without orchestration keep version 2 and stay
  byte-identical. Publish and teardown refuse a prefix whose index names
  another environment's or another incarnation's unit while that incarnation
  still exists in its environment's state, active, deleting, or retained. A
  target moves between environments once the old environment's incarnation is
  gone: torn down, or released with `nyl state forget` (see
  [Deletion](#deletion)). The first publish of the new owner then records
  itself in the index.
- Its dependencies are the units named by the target's `fromUnit` bindings and
  the PromotionRecords named by its `fromPromotion` bindings.
- Its execution key covers the target render's complete input record from
  Nyl's dependency recorder, plus `mode`: every file and directory listing,
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
  - Implementation status (M3): `releases` holds the bindings orchestration
    resolves, `fromUnit` and `fromPromotion`; `value`, `fromFile`, `fromGit`,
    and `fromPublication` inputs still enter the key through the render's
    input record, and join `releases` with the KubernetesPublication driver
    (M5) and PromotionPath selectors (M6). The renderer owns `releases`: an
    authored `spec.releases` is rejected.
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
  commit and ownership-index digest (`published`). `mode: observe` additionally
  records Argo CD observations and attests `accepted` and `healthy` (M5), per
  the promotion contract's [health evidence](promotion.md#health-evidence). Direct application through Nyl, which
  would attest `healthy` from rollout status during `reconcile`, is not a mode
  in the initial scope.

**Teardown.** A publication unit supports teardown when its target is
configured so that removing the manifests removes the workloads. It proceeds
in two phases, both through Git:

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

Implementation status (M3): the stand-in `FakePublication` kind carries out
both phases through Git on the real publication path
(`nyl_drivers::fake::publication`, with the tree changes in
`nyl_render::gitops::publication`), records the phase 1 commit as the wait's
`phase1Commit`, and removes in phase 2 exactly the files of its own
`_nyl/index.json`. The fake Argo CD observer (`FakeObserver`) stands in for
`observe` in tests. Publication target ownership (ownership index v3) ships
with the `KubernetesPublication` driver in M5; until then a teardown removes
only the files its own index records.

`teardownWait` decides when phase 1 counts as complete. The same point gates
phase 2 and the teardown of the units the publication depends on, such as the
IAM roles, subnets, or databases its workloads use, and through the reverse
dependency order the units those depend on in turn. Units with no dependency
path to the publication are not held back by its wait (see
[Deletion](#deletion)):

| Strategy | Phase 1 completes when | Default for |
| --- | --- | --- |
| `observe` | Argo CD no longer reports any of the target's workload Applications, within the unit's `timeout`. Otherwise the teardown is uncertain, with the reason `applications-remain` (listing them) or `observation-failed` (with the error, such as missing credentials for the Argo CD cluster) | `mode: observe` |
| `delay` | `duration` has passed since the phase 1 commit, such as `{strategy: delay, duration: 10m}`; the teardown is recorded as not observed | |
| `manual` | An operator confirms with `nyl teardown -e <env> --unit <u> --confirm-removed`; until then the unit is `tearing-down` and its dependencies wait, exit 2 | `mode: publish` |

`observe` needs read access to the Argo CD cluster, as `mode: observe` does.
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
`plan`, and a teardown agree without rendering. The driver reads them through
`Driver::teardown_settings`, and `rules::teardown_readiness` evaluates them:

| Requirement | Setting | If unmet |
| --- | --- | --- |
| The catalog prunes removed workload Applications on its own | catalog `syncPolicy.automated` enabled with `prune: true` | Blocking: the Applications stay until someone syncs the catalog with pruning |
| Deleting an Application deletes its workloads | catalog `applicationDeletionPolicy` is `Foreground` or `Background`, not `Orphan` | Blocking: workloads keep running without an Application |
| Owned Namespaces do not stop deletion | namespace `deletePolicy` is `Automatic` or `Retain`; the default `Confirm` renders `Delete=confirm`, which holds the Application's finalizer until someone confirms | Blocking: the Application and its workloads stay until the deletion is confirmed in Argo CD |
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
- The shared manifest lives under the reserved prefix `_nyl/shared/<name>/`
  with its own ownership index, owner `catalog:<name>`, so no target ever
  writes outside its own prefix. Nyl renders it from the `shared` block and
  the ArgoCDInstance's catalog defaults.
- The manifest has one writer in normal operation: the scheduled fleet
  reconcile of the template, which runs at the default branch's commit and
  Nyl version, republishes it when it differs. A target's publish creates it
  only when it does not exist yet, and otherwise warns when its own rendering
  would differ, so pull requests running different Nyl versions never
  overwrite each other. `nyl build` on the template renders it on demand, and
  a driver argument such as `-- --push` publishes it.
- Nothing is reconciled from state: Git holds the other targets' files, and
  a publish removes only files in the publishing target's index.
- Nyl renders the shared catalog, so the readiness table checks its settings.
  With `enabled: false`, Nyl cannot see the parent's sync and prune settings:
  the catalog rows become `external: not checked`, which the preflight reports
  without blocking, and `mode: observe` still checks that the Applications
  actually disappear.

Nyl reports unmet requirements where they matter:

- `nyl plan` and `nyl reconcile` warn for every `KubernetesPublication` whose
  `deletionPolicy` is `Teardown`, including every unit of a template instance,
  naming each unmet requirement and the setting that fixes it.
- `nyl state init --template` warns when the template contains publication
  units that are not teardown-ready, because instances are always torn down.
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
  keepSource: true                       # default: keep each instance's source commit fetchable
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
  template name, parameters, and expiry. Run again for an existing instance, it
  updates the parameters; the next reconcile applies them. It refuses a name
  already used by a declared Environment or by an instance of another
  template. Afterwards the instance behaves like a declared environment for
  every command.
- Instances are discovered by listing the template's state location, so no
  central index exists. Changing a template's `state` would hide the
  instances at the old location from the fleet job and the close jobs, so
  `plan` and `reconcile` refuse such a change while instances exist at the
  old location and name `nyl state move --template <name> --from …`, which
  moves every instance like `state move` moves one environment.
- `state init --template` records its entry worktree's commit as the
  instance's source commit, so a later plain run of the instance has one.
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
  template's ref pattern.
- With `keepSource` (the default), Nyl keeps each instance's latest source
  commit fetchable with a keep ref in the source repository,
  `refs/nyl/<instance>/keep` under the default `coordinationRefPrefix`, moved
  by every reconcile and removed by `state delete`. Teardown after the pull
  request branch is deleted or squash-merged therefore still finds its source.
  It needs push access to that ref pattern. `keepSource: false` turns it off, and the
  pipeline should then remove instances before their branches disappear.
- When an instance's recorded source commit cannot be fetched, teardown fails
  with a message suggesting `--source <revision>`. `nyl teardown --all` and
  `nyl state delete --teardown` accept `--source` to take the units' source
  files from another revision, typically the protected branch right after the
  pull request was squash-merged. Teardown uses the stored resolved specs, so
  only the source files come from that revision. When they differ from the
  recorded source, Nyl warns and records the substitution in the transition
  commit; it never refuses, so a lost commit cannot lock anyone out of
  removing an instance.
- An instance's units read shared infrastructure from declared environments
  with a cross-environment reference, such as
  `fromUnit: {environment: dev, unit: network, output: vpcId}`, under the
  rules of [Cross-environment references](#cross-environment-references),
  including the protection of the units they read.
- Instances cannot be a PromotionPath source; previews build and test, and
  promotion starts from declared environments.

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
  busy expired instance is removed by a later run. It never renders an
  instance's units with a template from another commit, because that
  combination was never reviewed together; pulling the default branch into
  every pull request remains a pipeline choice.

### Expiry

Nyl has no daemon, so expiry works through ordinary CI runs, and a continuous
runner (M7) could add timely enforcement later:

- `state.yaml` records `expiresAt`. Only activity on one instance extends it:
  `state init --template` and every `reconcile -e <instance>` without
  `--no-extend` move it to now plus `ttl` when they start, whether or not the
  run succeeds, so an active pull request keeps its preview even while its
  preview fails.
- Expiry is the declared intent to clean up, so every reconcile that finds an
  instance expired removes it instead of reconciling it: `reconcile -e
  <instance>`, `reconcile -e <instance> --no-extend`, and `reconcile
  --template <name>`. Removal runs `teardown --all` and then `state delete`.
  The template's `allowTeardown` authorizes that teardown; without it, the
  instance is reported as pending teardown and the run exits 2. An instance
  whose teardown readiness is incomplete is reported and left in place unless
  the run passes `--allow-incomplete`. Removal also waits for each
  publication's `teardownWait`: with the `manual` default of `mode: publish`,
  it stops after phase 1 until `--confirm-removed`, so templates meant to
  expire unattended use `observe` or `delay`, and `state init --template`
  warns otherwise. `status` and `get environments` show expired instances
  until they are removed.
- `--renew` is the only way to keep an expired instance: it extends the expiry
  and then reconciles.
- `reconcile -e <instance>` extends a live instance's expiry;
  `reconcile -e <instance> --no-extend` reconciles it as maintenance without
  extending; `reconcile --template <name>` never extends.
- `--renew` and `--no-extend` conflict and are rejected together. Both are
  rejected for declared environments, which have no expiry, and for the fleet
  reconcile, `--template` without `-e`.
- The pull request job's `reconcile -e <instance> --template <name> --param …`
  is activity by definition, so it renews an expired instance instead of
  removing it: updating an active pull request never removes its preview.
- A pull request pipeline runs `reconcile -e <instance> --template <name>
  --param …` on every push. It creates the instance when it is missing,
  updates its parameters when they changed, extends the expiry, and
  reconciles, so updating an active pull request never removes its preview. `state delete --teardown` removes one instance explicitly,
  expired or not.
- At `maxInstances`, `state init --template` refuses with exit 2 and names
  the expired instances that could be removed. Removing them is the scheduled
  fleet job's work, never a side effect of another pull request's job:
  hitting the limit is a project configuration concern, not the new pull
  request's. `--make-room` opts in to removing expired instances first, the
  same way the fleet job does, and then creating the instance.

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
| `PublishedTree` | `KubernetesPublication`, named `tree` | `repository` (publication URL), `branch` (string), `pathPrefix` (string), `commit` (commit ID), `indexDigest` (`sha256:<hex>` over the canonical JSON of `<prefix>/_nyl/index.json`) |

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
  units/<unit>/observations/<type>.yaml      # latest observation per type (drift, health)
  units/<unit>/attestations/<name>.yaml      # latest attestation per name for the current receipt
  attestations/<name>.yaml                   # environment-wide attestations for the current state
```

With separate refs, `desired/` exists only on the desired ref and `observed/`
only on the observed ref; `state.yaml` exists on both. `admission.yaml` exists
only on the admission ref, which is the desired ref unless `state.admissionRef`
names another.

Every state file is YAML. State records use `apiVersion: gitops.nyl/v1` with
the kinds `StateRecord`, `EnvironmentRecord`, `DesiredUnit`, `PromotionRecord`,
`ObservedUnit`, `Observation`, `Attestation`, `Lease`, `Run`, and `Signal`, plus
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
| `StateRecord` | `state.yaml` | `environment`, `template` and `params` for instances, `expiresAt`, `location` (repository, refs, `path`, `coordinationRefPrefix`), and `events`: `state-initialized` (with `fresh`), `state-moved`, `state-restored`, `state-deleted`, each with time, requester, and reason. A last event `state-deleted` is the deleted marker `nyl state delete` leaves (see [Instances](#instances)) |
| `EnvironmentRecord` | `desired/environment.yaml` | The resolved Environment spec with group defaults applied, `sourceCommit` S and how it was chosen (`entry`, `revision`, `commit`, `promotion` with the record's path and `sequence`, `admission` with the generation, or `override`), `appliedPromotionSequence`, and the ownership set as unit names with uids |
| `DeploymentAdmission` (M8) | `admission.yaml` on the admission ref | The current generation: ID, sequence, source commit and how it was chosen, semantic graph digest, and normalized authorizing actor, request, and reason; only a newer admission replaces it (see [Deployment admission](ci-execution.md#deployment-admission)) |
| `DesiredUnit` | `desired/units/<unit>.yaml` | See [Desired unit](#desired-unit); a `retained` tombstone is a DesiredUnit with `lifecycle.state: retained` that also embeds the last receipt, and has no observed file; a held marker is a DesiredUnit with `lifecycle.state: held` and only the unit's environment and name (see [Deletion](#deletion)) |
| `PromotionRecord` | `desired/promotions/<path>.yaml` | See the [promotion contract](promotion.md#promotionrecord) |
| `ObservedUnit` | `observed/units/<unit>.yaml` | See [Observed unit](#observed-unit) |
| `Observation` | `observed/units/<unit>/observations/<type>.yaml` | `unit`, `type` (`drift`, `health`), `executionKey`, `at`, `result`, driver-specific `details` |
| `Attestation` | `observed/units/<unit>/attestations/<name>.yaml`, `observed/attestations/<name>.yaml` | See [Attestations](#attestations) |
| `Lease` | the lease ref | `run`, `runner`, `operation`, `units` executing, `deadline`, `takenAt` |
| `Run` | the run ref's first commit, updated per phase; kept on an evidence ref after a takeover retires the run ref | `run`, `runner`, `operation`, `sourceCommit`, `readDesired`, `readObserved`, `deadline`, `units` executing, and `finished`; `created/<unit>.yaml`, the desired document and new uid of every unit the run created, recorded when the unit starts; and one checkpoint commit per finished unit holding its desired document, uid, and receipt or condition |
| `Signal` | the signal ref | `run`, `unit`, `action` (`confirm-removed` or `hold`), `phase1Commit` (for `confirm-removed`), `by`, `identity`, `reason`, `at`, `from` |

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
  what runs (`enabled`, `dependsOn`, `approval`, `deletionPolicy`, `timeout`),
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
- A new receipt removes the unit's `drift` observation in the same commit,
  because the observation was about the receipt it replaces (owner:
  `rules::drift::observation_changes`).

### Attestations

```yaml
apiVersion: gitops.nyl/v1
kind: Attestation
name: healthy
unit: {environment: dev, name: kubernetes, uid: 91c3…}   # absent for environment-wide attestations
executionKey: sha256:…                # the receipt attested; environment-wide: the desired commit of the state
result: pass                          # pass | fail
at: 2026-09-25T16:40:00Z
source: {driver: verify}              # or {driver: reconcile}, or {external: {by: alice, source: …, url: …}}
reason: null
details: {}                           # driver-specific, such as each Application's running publication
```

- Drivers report the attestations a unit declares from its resolved spec,
  besides `spec.attestations` entries with `from: external`; an Environment's
  `spec.attestations` declares environment-wide ones. The declared set is part
  of the desired unit, so a receipt knows what it can be attested for.
- A driver attests during `reconcile`, as part of its outcome, or during
  `verify`, next to its drift observation. `nyl attest` records external
  attestations under the lease, like every state-writing operation, and
  `--wait-lease` lets a CI job wait for a running reconcile; a refused name
  exits 1.
- An attestation is valid only for the receipt or state it names. Declared
  attestations of a new receipt are pending until attested; the newest per
  name and receipt wins. Git history keeps earlier ones, so an older state
  keeps the attestations it had.
- `evidence: attested` requires every declared attestation of the producer to
  pass; `attestations: [<name>, …]` requires the named ones instead. A failing
  attestation fails that requirement until a newer one passes. Promotion
  applies the same rules per unit and for the environment, as the
  [promotion contract](promotion.md#promotion-paths) describes.

**Consumer freshness.** A reference is satisfied only by a current producer
receipt. When a producer's desired document changes, its old receipt stops
being current, and every consumer that resolved from it becomes `blocked` on
that producer, even if the consumer already has its own receipt. The
consumer's existing receipt still documents what ran; the consumer executes
again only after the producer's new receipt exists and re-resolution changes
the consumer's execution key. A producer recreated with a new uid and the same
outputs therefore leaves its consumers current: re-resolution cites the new
receipt and the consumers' execution keys stay the same.

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
  latest drift observation was `drifted`, and `reapplied` records
  `{by, at, reason, units}` of a `--reapply`; each appears only when not
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
  `recover`, `verify`, `attest`, `exec`, `ci-admit`, `state-init`,
  `state-move`, `state-delete`,
  `state-forget`, and `state-restore`. `state reset` rewrites only the clone's
  local branches, and `lease break` writes only the lease ref; the
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
   current receipt is a candidate again when its latest drift observation for
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
   `teardown`, `hold`, `resume`, `recover`, `verify`, `attest`, `exec`,
   `ci admit`, `state init`,
   `state move`, `state delete`, `state forget`, `state restore`), so only one
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
   `verify` with nothing to verify, which writes nothing. `nyl hold` does not
   refuse a live holder: it delivers the hold to the running run through the
   run's signal ref (see [Deletion](#deletion)); a broken lease refuses it
   like every other non-importer.
2. **Deadline.** Every phase sets the deadline before it starts, plus a grace
   period that also absorbs clock skew, and no phase runs without one:
   resolution, including target renders and `fingerprint` scripts, by
   `resolutionTimeout` (`nyl.toml`, default 30 minutes); unit execution by the
   latest finish time of the units executing, by their `timeout`; `verify` by
   the timeouts of the units it verifies; a teardown wait by the wait's end.
   The runner updates the lease and the `Run` record together at each phase
   and unit start, with the batch's `created/` documents, in one atomic push
   that lands both or neither; a `--local` run whose lease is at the remote
   writes the two one after the other. There are no heartbeats.
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
     effects (`reconcile`, `teardown`, `exec`); a lease taken by `verify`,
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
     own: its phases renew only the lease. A `--local` run never imports,
     retires, or deletes remote run refs (see [Local runs](#local-runs)).
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
   `NYL-RUN-CANCELLED`, having written nothing: `reconcile` and `verify` exit
   4, `recover` and the state operations exit 1, like their other refusals.
   `verify` on a signal starts no further batch and releases the lease
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
`nyl state delete`, or has a local tip, `nyl recover -e <env>` still takes
the broken lease over, records the break where state exists, and releases
it, so `state init` (`--fresh` for deleted state), `state move`, or
`state restore` can run.

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
| `timeout` | A plan or verification passed its deadline; executions and teardowns that pass their deadline are uncertain instead | Yes |

A driver may set `retryable` explicitly for a single failure; the defaults
apply otherwise.

#### Drift

A receipt proves that an execution succeeded for its inputs; it does not prove
that the resources still match, because a change made outside Nyl leaves the
execution key unchanged. `nyl verify` asks each driver whether a current
receipt still holds and records the answer as the unit's `drift` observation,
tied to that receipt's execution key. Nyl does not watch for drift itself: a
pipeline runs `nyl verify` on a schedule, followed by `nyl reconcile`, until a
continuous runner (M7) takes over the cadence. A planned opt-in
`nyl reconcile --verify` (M7) does both under one lease and in one transition
commit; without it, reconcile never verifies.

- **`drifted` is a unit state.** A unit with a current receipt whose latest
  drift observation for that receipt is `drifted` is work for the next
  reconcile, like a changed unit: `nyl status` shows it `drifted` and exits 2
  for it, as for `ready`, and `nyl plan` lists it as a unit that would
  execute. Owners: `rules::selection` decides selection, `rules::drift`
  names repairs and the status attribute, and `rules::unit_state` the state.
- **Automatic repair.** With `driftRepair: converge`, the default, the next
  `reconcile` executes again every drifted unit whose recovery policy is
  `converge`, as it would execute a unit whose inputs changed. Its result is
  `executed`, its transition commit lists it under `driftRepaired`, and the
  new receipt replaces the one the observation was about and removes the
  observation in the same commit, so it is not repaired again until a later
  `verify` finds new drift. Idempotent units therefore repair drift without
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
- **Verification warnings.** `nyl verify` reports each drifted unit with
  `NYL-VERIFY-DRIFT`, whose fix says, per unit, what the next reconcile does:
  it repairs the unit, the unit awaits approval, or, under `manual`, it needs
  `nyl reconcile --unit <u> --reapply --reason <text>`.
- The walkthrough [`drift`](#walkthroughs) shows `verify` finding drift,
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
  fence. ObservedUnit records this marker and the exact native scope accepted
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
  stays readable; its observed file, observations, and attestations are
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
    `verify`, `recover`, or a state operation, `nyl hold` meets a held
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

Runs from a workstation are ordinary runs. Without CI wired up yet, `nyl
reconcile -e <env>` from a workstation takes the remote lease and pushes
transition commits to the remote state exactly like a CI job; its source
commit must be pushed and reachable per [Pinned commits](#pinned-commits). CI
later continues from the state those runs recorded.

Nyl keeps the clone's own branches of the state refs, `nyl/<env>/desired` and
`nyl/<env>/observed` by default, as the local known version of the state:

- Every command that fetches an environment's remote state fast-forwards those
  local branches, creating them if needed. It never rewrites a local branch
  that has diverged, and it sets no upstream, so a bare `git push` does not
  publish them. `git log nyl/<env>/desired` works in any clone that ran Nyl.
- **No remote.** When the state repository resolves to the local repository,
  because nothing is configured and the checkout has no `origin`, the local
  branches are the state. Runs are ordinary runs with a local lease;
  `nyl state init -e <env>` creates the state there.
- **Adding a remote.** Once the state repository resolves to a remote, a run
  finds no `state.yaml` there, refuses, and names `nyl state move -e <env>
  --from local`. The move pushes the branches' history to the remote, writes
  the move event there as the new tip, with `state.yaml` naming the remote,
  and fast-forwards the local branches to it. Old and new location share one
  history, so a run with the old configuration reads the new location from
  `state.yaml` and refuses; no marker is needed at the old location.

`--local` runs orchestration next to remote state without publishing it, for
experiments from a workstation:

- A local run continues from the local branches and writes its transition
  commits to them with a `Nyl-Local: true` trailer, never pushing state. It
  needs state refs that have local branches: short names or `refs/heads/…`.
  State without a remote, whose branches already are the state, and full
  `refs/…` state refs outside `refs/heads/` refuse `--local` with
  `NYL-LOCAL-RUN-UNSUPPORTED`. While
  the remote does not move, the local branches are the remote state plus the
  local runs; once it moves, they have diverged, Nyl stops fast-forwarding
  them, and `nyl status -e <env>` reports it, such as "local state has 2
  local transitions and is 3 behind the remote". `nyl status` and `nyl get`
  with `--local` show the local view.
- A local run takes the remote lease (`refs/nyl/<env>/lease`), because the lease is
  coordination, not state: a local run and a CI run of the same environment
  never execute at the same time. This needs push access to the lease ref.
  The lease it holds lists no units, and its run ref stays in the clone,
  because remote runs do not notice local effects and a takeover of a
  crashed local run must not mark remote units uncertain. Live remote run
  refs still block it like a held lease.
- A local run never imports remote run refs, so it refuses with
  `NYL-LOCAL-RUN-LEFTOVERS` while the remote holds run refs of dead runs that
  no remote run has taken over yet: finished, past their deadline, or the
  holder of a broken lease. It checks before taking the lease, leaving the
  lease and any break as they were; a remote `reconcile` or `recover` takes
  them over first. It never retires or deletes remote run refs or evidence refs.
  Under the remote lease, it also refuses a broken remote lease whose run is
  not imported with `NYL-LEASE-BROKEN`, exit 2, naming `nyl recover -e
  <env>`, even when the broken holder left no run ref, because only an import
  records the break (owner: `shell::lease::run_takeover`).
- `--local-lease` explicitly opts into a local lease instead, for working
  without that access, and accepts that CI may run at the same time. It
  reads only the clone's own lease and run refs.
- Effects are real, and local state is never pushed, so remote runs do not
  notice them: a unit whose remote receipt is current is not executed again,
  and the remote state keeps describing what ran before. A local run next to
  remote state warns about every unit it executes and names the remote
  command that records it again, such as `nyl reconcile -e <env> --unit
  <u>` after the change is committed (`NYL-LOCAL-RUN-UNRECORDED`). Units
  whose recovery policy is not `converge` run locally only when named with
  `--approve <unit>`; others report `awaiting-approval`, exit 2, like any
  approval wait. The executed unit's receipt and the local transition
  commit's summary record `approval: {by: [<commit identity>], identity:
  local, source: local, digest: null, reviewed: false}` by default; explicit
  identities and configured lookups follow the common approval rules. Without
  a commit identity nothing is approved.
- `nyl state reset -e <env> --local` drops the local transitions and follows
  the remote again, once the remote has recorded the change or the local
  effects were undone. It lists the units whose local receipts the remote
  does not reflect, because forgetting them can leave resources behind, and
  proceeds only with `--discard <unit>…` or `--discard-all`.
- A run against remote state refuses when the tip of a remote state ref is a
  `Nyl-Local` commit, which only a manual push of local branches produces;
  Nyl itself commits only on tips that pass this check. The refusal,
  `NYL-STATE-LOCAL-TIP`, names the last commit without the trailer. The check
  runs in every operation that writes remote state: `reconcile`, `verify`,
  `recover`, `state init --fresh`, `state forget`, and `state move` at both
  locations. On refs several environments share through `path`, each
  environment looks only at its own commits, those whose `Nyl-Environment`
  trailer names it. A store that keeps no history passes. `nyl state restore -e <env> --to <commit>`
  writes a forward commit restoring that commit's content, so protected
  branches need no force push, and records the local transitions as rejected.
  It never restores `admission.yaml`: the current deployment admission is
  carried forward, so a restore cannot move the deployment generation back.
  With separate desired and observed refs, the commit on the other ref is
  paired with `--to` by the same `Nyl-Run-Id`, else by the
  `Nyl-Read-Desired` or `Nyl-Read-Observed` trailer of `--to`. A `--to` or
  paired commit that is a `Nyl-Local` commit, that belongs to another
  environment, or that cannot be paired is refused with
  `NYL-STATE-RESTORE-INVALID`.
  Pushed local transitions are never accepted, because their source commits
  may be unpushed or dirty and were never checked for reachability.

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
    fn execution_key_inputs(&self, ctx: &ResolutionContext, spec: &ResolvedSpec) -> Result<KeyInputs>; // selected files, excluded fields, fingerprint

    fn plan(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<PlanReport>>;
    fn reconcile(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Outcome>>;
    fn verify(&self, ctx: &ExecutionContext, unit: &DesiredUnit, receipt: &Receipt) -> Result<Supported<Verification>>; // drift, observations, attestations
    fn inspect(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Inspection>>;
    fn teardown(&self, ctx: &ExecutionContext, unit: &DesiredUnit) -> Result<Supported<Outcome>>; // lifecycle: deleting
    fn teardown_settings(&self, unit: &DesiredUnit) -> Result<Supported<TeardownSettings>>; // readiness from the stored document; default Unsupported
    fn build(&self, ctx: &ExecutionContext, unit: &DesiredUnit, push: bool) -> Result<Supported<Vec<Artifact>>>; // outside orchestration
    fn exec(&self, ctx: &ExecutionContext, unit: &DesiredUnit, args: &[OsString]) -> Result<Supported<ExecStatus>>; // operator repair, see Operator exec
}

struct Capabilities {
    plan: bool, reconcile: bool, verify: bool, inspect: bool, teardown: bool,
    observe: bool,              // verify records observations, such as Argo CD health
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
| `Verification` | `drift`: `clean`, `drifted` with a summary, or `unknown` with a reason; `observations`; `attestations` |
| `Inspection` | `Applied { outputs, artifacts }`, `NotApplied`, or `Unknown { reason }`, the three results of the `inspect` recovery policy |
| `TeardownSettings` | The target settings teardown readiness checks: whether the catalog prunes, the Application deletion policy, each owned Namespace's delete policy, and whether a parent owns the catalog; a setting Nyl cannot see is `None` and reported as not checked |
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
- `teardown_settings` reads the stored desired document only, so `status`,
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
  the unit's observed file, and a transcript sink that masks secret values.
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
  because effects may have happened; a plan or verification ended this way
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
  files: ['../scripts/seed/**']
  values:
    bucket: {fromUnit: {unit: storage, output: bucket}}
  command: ['./run.sh']
  verify: ['./verify.sh']
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
  timeout: 10m
```

- `command`, `verify`, `teardown`, and `fingerprint` are argument vectors,
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
    results, when the unit declares attestations;
  - `NYL_UNIT`, `NYL_ENVIRONMENT`, and `NYL_OPERATION` (`reconcile`,
    `verify`, `teardown`, or `fingerprint`);
  - variables and secrets admitted through the common `env` field (see
    [Credentials](#credentials)).
- Every step has the unit's `timeout` and the termination rules of
  [Drivers](#drivers): a `command` or `teardown` that passes it, or dies from
  a signal, is uncertain; a `verify` or `fingerprint` that passes it fails.
- Following the general rule, the execution key covers `command`, `verify`,
  `teardown`, `fingerprint`, `workingDir`, `idempotent`, `outputs`, the files matched by `files`,
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
- `verify` exits 0 for clean, 2 for drift, and anything else for an error. It
  records a drift observation, never a receipt, and it receives the same
  inputs and admitted environment as `command`, with `NYL_OUTPUTS` absent.
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
  `attestations: [{name: healthy}]` for a smoke test. The command, or its
  `verify`, writes their results to `NYL_ATTESTATIONS` as one JSON object of
  `pass` or `fail` per name; a declared name it leaves out stays pending.
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
| `verify` | Run driver verification against current receipts | One observed commit with the latest observations and driver attestations |
| `attest` | Record an external attestation for a unit's current receipt or the environment's current state (see [Attestations](#attestations)) | One observed commit |
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
| `state reset` | With `--local`, drop local transitions and follow the remote state again (see [Local runs](#local-runs)) | The clone's local state branches only |
| `state restore` | Restore remote state to an earlier commit's content after local transitions were pushed by hand | One desired and one observed commit |
| `state forget` | Drop a `retained` tombstone or a `pending-teardown` unit without touching resources | One desired and one observed commit |
| `lease break` | Declare a lease holder gone so the next run takes over at once | The lease ref; recorded in the transition commit that imports the broken run |
| `build` | Run a unit's `build` capability from the current worktree, outside orchestration (see [Direct builds](#direct-builds)) | Nothing in state; the artifact is published only through driver arguments (for example `-- --push`) where the kind allows it |
| `promote` | Record a proven source state's values in the target environment (see the [promotion contract](promotion.md)) | One desired commit with the PromotionRecord, or a pull request under `changeGate: pullRequest` |

All are top-level `nyl` commands. `release` is taken by Kubernetes release
history and `delete` by source editing, so lifting a hold is `resume` and
tearing down is `teardown`.

### Multi-environment runs

`plan`, `reconcile`, `status`, and `verify` accept several environments in one
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
- `nyl get output <unit>/<output>` and `nyl get artifact <unit>/<name>
  --pointer …` return single values for scripts and resolve exactly like the
  matching `fromUnit` reference: the same pointer base (inside the artifact's
  `spec`), the same digest check, and the same freshness rule. When the
  reference would not resolve (no current receipt, or a missing output,
  artifact, or pointer target), they print nothing and exit 1. `--allow-stale`
  prints the latest receipt's value with a warning instead.
- `--state-revision <commit>` reads state as of an earlier state commit, the
  same name `nyl promote` uses for a state commit; `--local`
  reads the clone's local state branches, including local runs. Output formats: `table` (default), `wide`,
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
`--allow-teardown`, `--allow-dependents`, `--allow-incomplete`, `--confirm-removed`, `--wait-lease`, `--make-room`, `--renew`, `--no-extend`, `--template`, `--source`, `--local`, `--local-lease`,
`--leaving`, `--generation`,
`--concurrency`, and `--output json` for versioned machine results on
stdout, with human diagnostics on stderr.

Each unit has one state in `status`: `blocked` (with the unresolved reference or
non-current dependency), `ready`, `drifted` (a current receipt whose latest
drift observation is `drifted`), `awaiting-approval`, `running` (with the run
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
| 3 | At least one execution or verification failed |
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
| `verify` | Every verified unit is clean, and its attestations pass | Error, including a verification that passed its unit's deadline | A unit could not be verified: no current receipt, a kind that cannot verify, or `unknown` drift | Drift, or a failing attestation | — |
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

Reconcile, teardown, verify, and status reports have the same shape, with
`result` per unit as in the transition commit summary. The report's
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
its lease with `nyl lease break` instead of waiting for the deadline. `verify`
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
idempotent Command and of one that is not. `verify` records both as drifted,
exit 3 with `NYL-VERIFY-DRIFT`; `status` shows them `drifted`, exit 2, and
`plan` lists them as units that would execute. The next `reconcile` repairs
the idempotent unit, listed under `driftRepaired`, and reports the other
`awaiting-approval`, exit 2; a `--local` run executes it with `--approve`.
Under `driftRepair: manual`, `reconcile` repairs nothing and the unit stays
`drifted` until `reconcile --unit idempotent --reapply --reason …`, recorded
under `reapplied`.
([`drift`](../nyl/tests/scenarios/walkthroughs/drift/scenario.yaml))

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

Driver-specific questions are in the [infrastructure units contract](infrastructure-units.md).

