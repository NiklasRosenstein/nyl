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
| Environment, EnvironmentTemplate, PromotionPath (and the existing GitRepository) | `gitops.nyl/v1` |
| Built-in unit kinds: `Command`, `Terraform`, `OpenTofu`, `OciImage`, `KubernetesPublication` | `units.gitops.nyl/v1` |
| Artifact kinds: `ContainerImage`, `PublishedTree` | `artifacts.gitops.nyl/v1` (see [Artifacts](#artifacts)) |
| Plugin unit and artifact kinds | The plugin's own groups, such as `units.acme.example/v1` (see [Plugin drivers](#plugin-drivers)) |

Every resource in a unit group is a unit; the group identifies the family and
the kind selects the driver, as `components.k8s.nyl/v1` does for component
invocations. Discovery follows Git visibility across the project, like the
rendered GitOps resources. `nyl create` and `nyl delete` gain `environment`,
`unit`, and `promotion-path` resources; `nyl get` covers those declarations
and, with `-e`, an environment's units, outputs, artifacts, and promotions
(see [Inspection](#inspection)).

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
    path: ''                                # optional directory within the refs; default the root
  protectedRefs: [main, 'release/*']        # default: each repository's default branch
  allowUnprotectedSource: false             # true lets runs use commits outside protectedRefs
```

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
  the source checkout's `origin` remote; CI should name a GitRepository.
- `desiredRef` and `observedRef` may name the same ref; the layout is identical
  either way (see [State layout](#state-layout)).
- `path` places the environment's state in a directory of those refs, so several
  environments can share one ref, each with its own `state.yaml`, `desired/`,
  and `observed/`. Leases stay per environment, so environments sharing a ref
  still run in parallel; their pushes serialize on the ref and, touching
  disjoint paths, rebase under the transition commit conflict rule. Branch
  protection and retention are then shared.
- `protectedRefs` lists the refs that pinned commits and run source commits
  must be reachable from; `allowUnprotectedSource` relaxes that for run source
  commits (see [Pinned commits](#pinned-commits)).

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
  `approval` (see [Approval](#approval)), `deletionPolicy` (`Teardown` or
  `Retain`; see [Deletion](#deletion)), `timeout`, and `env` (see
  [Credentials](#credentials)). They are one Rust struct flattened into each kind's spec, so
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
  move through the secrets provider, not through outputs. Undeclared outputs
  are ignored by every kind, sensitive or not; the Command kind warns about
  them. A declared output that the native tool marks sensitive must be
  declared `sensitive`, and drivers check this before any effect where the
  tool allows.
- `dependsOn` lists units, by name or as `{unit, evidence}`, that must have a
  current receipt first, for ordering without a data reference. References add
  dependencies implicitly.
- A `dependsOn` entry or a `fromUnit` reference may require stronger evidence
  from its producer with `evidence: published | accepted | healthy` (default
  `published`, which is a current receipt). `accepted` and `healthy` use the
  levels defined in the roadmap's health evidence section, so they apply only
  to producers that record them, such as a `KubernetesPublication` in
  `mode: observe`; requiring them from any other producer is a resolution
  error. A consumer whose producer lacks the evidence is `blocked` on it:

  ```yaml
  dependsOn:
    - {unit: kubernetes, evidence: healthy}   # smoke tests run against healthy workloads
  ```
- Repository content from elsewhere uses the ApplicationGroup source shape in
  the kind's `source` field: `repositoryRef` or `repository`, a human
  `revision`, a locked `commit`, and `path`. `nyl update source-locks` refreshes
  these locks with the others. Without a repository, `source.path` is read at
  the source commit.

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

### References

| Reference | Resolves to | Freshness |
| --- | --- | --- |
| `fromUnit: {unit, output, pointer, evidence}` | A declared, non-sensitive output of a unit in the same environment; `pointer` optionally selects inside an `object` or `array` output | The producer's receipt must be current, with the required `evidence` |
| `fromUnit: {unit, artifact, kind, pointer, evidence}` | A field of an artifact the unit published; `kind` optionally asserts the artifact kind | The producer's receipt must be current, list the artifact's digest, and have the required `evidence` |
| `fromPromotion: {path, value}` | A value in a PromotionRecord in this environment's desired state | The record must exist; broken selectors are errors |
| `fromUnit: {environment, unit, output \| artifact, …}` | An output or artifact of a unit in a declared environment, read from a template instance only (see [Environment templates](#environment-templates-and-previews)) | As `fromUnit`, against the other environment's current receipt |

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
  teardown. Static DeploymentTargets remain for rendering without
  orchestration.
- Generated Argo CD Application and AppProject names must be unique among all
  targets whose Argo CD instance resolves to the same cluster and namespace:
  static targets on explicit or implicit instances, and inline targets alike.
  Validation checks this across every target it can see, including every
  instance of an EnvironmentTemplate, whose inline targets must therefore
  template their names, for example from `environment.name`. Extending today's
  check, which compares only targets sharing an explicit ArgoCDInstance, is an
  M2 item.
- The unit places its DeploymentTarget into the environment. A target
  referenced by publication units in two environments is an error; a target
  referenced by none rejects `fromUnit` and `fromPromotion` bindings.
- The ownership index of every tree the unit publishes records the owning
  environment and unit uid next to the existing target, cluster, and
  publication identity. Publish and teardown refuse a prefix whose index names
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

**Teardown.** A publication unit supports teardown when its target is
configured so that removing the manifests removes the workloads. It proceeds
in two phases, both through Git:

1. **Remove the workload Applications.** The unit publishes a tree whose
   `_nyl/catalog` still contains the catalog Application and the generated
   AppProjects but no workload Applications; the workload trees stay. The
   catalog is never empty, so Argo CD's empty-tree guard and the catalog's own
   self-prune policy never come into play, and the AppProjects outlive the
   Applications that use them. This commit is the teardown's `published`
   evidence. Argo CD's catalog sync prunes the workload Applications, and
   their finalizers delete the workloads.
2. **Remove the rest.** Once phase 1 counts as complete under `teardownWait`,
   the unit removes the remaining files it owns: exactly the files recorded in
   `_nyl/index.json`, which includes the workload trees and the catalog.
   Unowned files in the prefix, such as state files committed by other tools,
   are preserved, as in every reconciliation.

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
  dependencies' teardown run at once, and the teardown is recorded with a
  condition saying removal was not confirmed.

`status` suggests both for an uncertain wait: fix the observer configuration
and retry, or confirm after checking by hand.

Teardown readiness is checked statically from the target's effective settings:

| Requirement | Setting | If unmet |
| --- | --- | --- |
| The catalog prunes removed workload Applications on its own | catalog `syncPolicy.automated` enabled with `prune: true` | Blocking: the Applications stay until someone syncs the catalog with pruning |
| Deleting an Application deletes its workloads | catalog `applicationDeletionPolicy` is `Foreground` or `Background`, not `Orphan` | Blocking: workloads keep running without an Application |
| Owned Namespaces do not stop deletion | namespace `deletePolicy` is `Automatic` or `Retain`; the default `Confirm` renders `Delete=confirm`, which holds the Application's finalizer until someone confirms | Blocking: the Application and its workloads stay until the deletion is confirmed in Argo CD |
| Owned Namespaces are removed | namespace `deletePolicy` is `Automatic` | Remnant: empty Namespaces remain |
| The catalog Application and AppProjects are removed | a parent Application owns the catalog Application, for example over a shared preview branch | Remnant: the catalog Application and AppProjects remain after phase 2 |

The three blocking requirements decide whether teardown can succeed; the
remnant rows leave objects behind. Nyl reports unmet requirements where they
matter:

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
  reasons, exit 2. An incomplete teardown is recorded with a condition listing
  what may remain, so state never claims a clean teardown that did not happen.
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
    - {name: pr, type: integer}
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

The template has the Environment's fields plus `parameters` (typed like Release
inputs and exposed as `params`), `deletionPolicy`, `allowTeardown`, `ttl`,
`maxInstances`, and `keepSource`. The template's `deletionPolicy: Teardown`
overrides every unit's own policy, including an explicit `Retain`, for kinds
that support teardown. Units of other kinds, such as `OciImage`, are left in
place and dropped from state when an instance is removed, and the transition
commit lists what was left, such as the image reference. Removing unused
images is the registry's retention policy's job. The template knows nothing
about Kubernetes; a preview's cluster and publication come from
`KubernetesPublication` units with inline targets.

### Instances

Instances use the ordinary commands:

```bash
nyl state init -e pr-123 --template preview --param pr=123   # create or update an instance
nyl reconcile -e pr-123
nyl get units -e pr-123 / nyl status -e pr-123
nyl teardown -e pr-123 --all                                 # tear down every unit
nyl state delete -e pr-123                                   # remove the instance's state
nyl get environments                                         # declared environments and instances
```

- `nyl state init --template` writes the instance's `state.yaml` with the
  template name, parameters, and expiry. Run again for an existing instance, it
  updates the parameters; the next reconcile applies them. It refuses a name
  already used by a declared Environment or by an instance of another
  template. Afterwards the instance behaves like a declared environment for
  every command.
- Instances are discovered by listing the template's state location, so no
  central index exists.
- `nyl teardown -e <env> --all` tears down every unit, dependents first, and
  holds each unit that is still in the ownership set, so nothing is recreated.
  It also decommissions declared environments: run `teardown --all`, then
  remove the Environment from source. Units whose kind has no teardown support
  are left in place and dropped from state. Units that depend on a publication
  are torn down only once the publication's teardown completes under its
  `teardownWait`, so with the `manual` strategy `teardown --all` stops after
  the publication and exits 2 until `--confirm-removed`.
- `nyl state delete -e <env>` removes an instance's state: its directory in a
  shared ref, or its refs. It works only for template instances whose units
  have no incarnation left, and it removes everything, including records of
  units that were left in place; `--teardown` runs `teardown --all` first. History
  keeps the removed state. The recommended layout for previews is a shared
  ref with `state.path`, so `state delete` removes a directory instead of a
  ref; separate refs per instance need permission to delete refs matching the
  template's ref pattern.
- With `keepSource` (the default), Nyl keeps each instance's latest source
  commit fetchable with a keep ref in the source repository,
  `refs/nyl/keep/<instance>`, moved by every reconcile and removed by
  `state delete`. Teardown after the pull request branch is deleted or
  squash-merged therefore still finds its source. It needs push access to that
  ref pattern; hosts that reject custom refs can use
  `refs/heads/nyl/keep/<instance>`. `keepSource: false` turns it off, and the
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
  `fromUnit: {environment: dev, unit: network, output: vpcId}`. It is
  read-only, allowed only from template instances to declared environments,
  and blocks like any reference when the producer's receipt is not current.
- Instances cannot be a PromotionPath source; previews build and test, and
  promotion starts from declared environments.

### Template changes

An instance renders its template and units at its own source commit S,
typically the pull request's head, so a preview always shows what that pull
request would deploy.

- A template change on the default branch reaches an instance when the pull
  request picks it up and CI reconciles the new head. Changed values, units,
  or inline targets change execution keys, so exactly the affected units run
  again; a unit removed from the template is torn down, without
  `--allow-teardown` when the template sets `allowTeardown`.
- `nyl reconcile --template preview` reconciles every instance, each at its own
  recorded source commit: after a Nyl upgrade that changes rendered output, a
  driver behavior-version change, or new evidence in shared infrastructure. It is maintenance, not
  activity: it never extends expiry, and it removes expired instances (see
  [Expiry](#expiry)). It never renders an
  instance's units with a template from another commit, because that
  combination was never reviewed together; pulling the default branch into
  every pull request remains a pipeline choice.

### Expiry

Nyl has no daemon, so expiry works through ordinary CI runs, and a continuous
runner (M7) could add timely enforcement later:

- `state.yaml` records `expiresAt`. Only activity on one instance extends it:
  `state init --template` and a successful `reconcile -e <instance>` move it to
  now plus `ttl`, so an active pull request keeps its preview.
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
  rejected for declared environments, which have no expiry, and with
  `--template`.
- A pull request pipeline runs `state init --template` before reconciling,
  which extends the expiry, so updating an active pull request never removes
  its preview. `state delete --teardown` removes one instance explicitly,
  expired or not.
- At `maxInstances`, `state init --template` first removes expired instances of
  that template the same way; if none are expired, it refuses.

A typical pipeline runs `state init --template` and `reconcile -e` when a pull
request opens or updates, `state delete --teardown` when it closes, and
`reconcile --template` on a schedule, which both maintains live instances and
removes expired ones.

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

- `sourceRevision` is S unless the unit's `source` field names a repository or
  carries a promoted revision. Resolution always writes it into
  `resolvedSpec.source.commit`, so a PromotionPath selector on `/source` carries
  the path together with the exact commit the source environment executed.
- `resolvedSpec` is what PromotionPath selectors such as `input: /source` or
  `input: /releases/platform~1web/image` point into.
- `provenance` records, per reference, the receipt or PromotionRecord the value
  came from, citing the observed commit it was read from. A value from a
  receipt produced earlier in the same run cites the run ID instead, which
  identifies the transition commit through its `Nyl-Run-Id` trailer; run refs
  are disposable, so provenance never points at a checkpoint. Provenance never
  names the commit it is written in.
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
- Operations: `reconcile`, `promote`, `teardown`, `hold`, `resume`,
  `recover`, `verify`, `state-init`, `state-move`, `state-delete`,
  `state-forget`.
- Replaying operations in order, from the read commits and evaluation times
  they name, reproduces every state decision.

## Execution

`nyl reconcile -e <env>`:

1. **Lease.** Take the environment's run lease (see
   [Runs and leases](#runs-and-leases)).
2. **Resolve.** Render the environment's units at source commit S and resolve
   references against current receipts and PromotionRecords, then import
   results left behind by earlier runs against that resolution.
3. **Select.** A unit is ready when its desired document is `ready` and
   `active`, it has no current receipt, its provenance receipts are all still
   current, and it is not awaiting approval. A unit with an `uncertain`
   condition is selected only when its driver's recovery policy allows it:
   `converge` executes it again, and `inspect` runs `inspect` first (see
   [Recovery](#recovery)). Under `manual`, the unit waits for `recover`. A unit
   with a non-retryable `failed` condition is not selected until
   `recover --retry` clears it.
4. **Wave.** Execute ready units, independent units in parallel up to a
   concurrency limit, and checkpoint each result on the run ref.
5. **Re-resolve.** New receipts may unblock dependents. Resolve again at the
   same S.
6. **Repeat** from step 3 until nothing is ready.
7. **Commit.** Write one transition commit per state ref, fenced by the lease,
   then release the lease and delete the run ref.

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
  accepts `--approve` like `reconcile`.
- The approved digest reaches the driver through `ExecutionContext`, and the
  driver compares it with the plan it is about to apply. A mismatch returns
  `Outcome::AwaitingApproval` with the new digest: no effects, the unit waits,
  and the run exits 2.

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
check:

| Ref | Content | Lifetime |
| --- | --- | --- |
| `nyl/<env>/lease` | One `Lease`: run ID, runner, operation, deadline, units executing | Created at run start, deleted at run end |
| `nyl/<env>/runs/<run-id>` | The run's `Run` record and a checkpoint commit per finished unit: its receipt or failure, and its artifacts | Deleted after its results reach a transition commit |

1. **Lease.** A run creates the lease ref with compare-and-swap. If a lease
   exists and has not expired, the run exits 2 and reports who holds it. Every
   state-writing operation takes the lease (`reconcile`, `promote`,
   `teardown`, `hold`, `resume`, `recover`, `verify`, `state init`,
   `state move`, `state delete`, `state forget`), so only one runs per
   environment at a time.
2. **Deadline.** The deadline is the latest finish time of the units executing,
   by their `timeout`, plus a grace period that also absorbs clock skew. The
   runner updates the lease when it starts a unit; there are no heartbeats.
3. **Checkpoints.** Each finished unit is checkpointed on the run ref before
   the next wave, together with the desired document it executed and its uid,
   including documents the run created. A crash loses no evidence.
4. **Takeover.** A run that finds an expired lease replaces it with
   compare-and-swap. After resolving, it imports every leftover run ref. A
   checkpointed result becomes a receipt when the new resolution of that unit
   has the same execution key; for a unit the dead run created, the new run
   adopts the checkpointed desired document and uid instead of generating a
   new one. Other results are reported as superseded; their effects are real,
   so a superseded result of a unit whose recovery policy is not `converge`
   gives it an `uncertain` condition rather than letting it run again. Units
   the dead run was executing without a checkpoint get an `uncertain`
   condition. All of this lands in the new run's transition commit, under
   `imported` and `recovered`.
5. **Fenced commit.** The final transition commit is pushed with
   `git push --atomic`, together with a compare-and-swap of the lease ref that
   expects the run's own lease commit, so either both land or neither does.
   Releasing the lease is a compare-and-swap too. A run whose lease was taken
   over pushes nothing to the state refs and exits 4; its results stay on its
   run ref for the next run to import. Atomic pushes work on GitHub, GitLab,
   and plain Git servers.
6. **Lost lease.** A runner whose lease update fails stops starting units,
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
  state: deleting                    # active | deleting | held | retained
  deletion:
    reason: omission                 # omission | teardown | replace | kind-change
    intent: teardown                 # retain | teardown
    requested: {by: alice, at: 2026-09-24T12:00:00Z, reason: decommission}
```

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
  them. A `KubernetesPublication` counts as torn down only once its
  `teardownWait` completes, so the units it depends on, and transitively
  theirs, wait for it. Units with no dependency path to a waiting unit are
  torn down in the same run without waiting. Teardown caused by omission
  requires `--allow-teardown` on `reconcile`; without it the unit is
  `pending-teardown` and the run exits 2, so an unintended omission, such as a
  selector typo, destroys nothing, and fixing it returns the unit to `active`
  with its uid and receipt. When teardown succeeds, the run removes both
  files.
- **Retain, declared.** A unit that declares `deletionPolicy: Retain` keeps
  its resources when it leaves the ownership set. The run replaces its desired
  and observed files with a `retained` tombstone: the last desired document,
  uid, and receipt, under `lifecycle.state: retained`. A unit that returns with
  the same name and kind adopts the tombstone: with an unchanged execution key
  it is current at once, otherwise it runs as an ordinary change of the same
  incarnation, so restoring an accidentally omitted unit never re-runs it.
  `nyl status` lists tombstones, and the transition commit's summary lists
  every newly retained unit.
- **Retain, by kind.** A unit whose kind cannot tear down and that does not
  declare `Retain` leaves no tombstone: the run removes its files and lists
  what was left in place, such as an image reference, in the transition
  commit's summary. Returning later starts a new incarnation.
- **Forgetting.** `nyl state forget -e <env> --unit <u>` drops a `retained`
  tombstone or a `pending-teardown` unit from state without touching its
  resources, for example to let a Teardown unit go without destroying it.
  A returning unit then starts a new incarnation.
- **Explicit teardown.** `nyl teardown -e <env> --unit <u>` tears a unit down
  whether or not it is in the ownership set. The command is itself explicit
  intent, so it needs no `--allow-teardown`.
  - For a unit outside the ownership set (a `deleting` unit, or a `retained`
    tombstone, whose last desired document it holds), it completes the
    deletion and removes the files, including the tombstone.
  - For a unit still in the ownership set, it replaces the unit
    (`reason: replace`): the incarnation is torn down, and the next
    `reconcile` writes a new desired file with a new uid. Dependents are
    blocked until the new receipt exists, then run with its outputs. This
    rebuilds corrupted resources or rotates something that can only be
    recreated.
  - `--hold` holds the unit in the same operation, so no run can recreate it
    between the teardown and a separate `nyl hold`. It is equivalent to
    `teardown` followed by `hold`.
- **Holds freeze a unit.** `nyl hold -e <env> --unit <u> [--reason <text>]`
  makes `reconcile` change nothing about the unit, whatever state it is in,
  without touching its resources. Resolution records what source would change
  under `hold.pending`, with its source commit and execution key, but applies
  nothing:
  - A unit with a running incarnation keeps its last executed desired
    document. Its receipt stays current against that document, so consumers
    keep resolving from it; the frozen state is what actually runs.
  - A unit without an incarnation (after a teardown, or declared but never
    executed) is not created. Its desired file has no uid, and its dependents
    stay blocked.
  - A unit that is `deleting` pauses: no retention or teardown happens.
  - A held unit that leaves the ownership set is not deleted; it is reported
    as held with a pending deletion.
  - An explicit `nyl teardown` of a held unit is still allowed, because it is
    an operator action; the unit stays held afterwards and is not recreated.
    A held unit's desired file survives its teardown (held, without a uid),
    even outside the ownership set, and is removed only after `resume`.
  - `nyl resume -e <env> --unit <u>` lifts the hold. The next `reconcile`
    renders the unit from source again and applies whatever changed, including
    a pending deletion.
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
  kind change under the same name is a deletion with `reason: kind-change`; a
  `retained` tombstone of another kind is dropped, with its resources left in
  place, and the new kind starts a new incarnation.

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
- When an Environment's `state` points to a location without `state.yaml`,
  `reconcile` refuses and names both ways forward:
  - `nyl state move -e <env> --from <repository> [--desired-ref …]
    [--observed-ref …]` relocates the state. It holds the lease at both
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
- A location whose `state.yaml` names a different environment is an error, so
  two environments can never share state by accident.

### Local runs

Runs from a workstation are ordinary runs. Without CI wired up yet, `nyl
reconcile -e <env>` from a workstation takes the remote lease and pushes
transition commits to the remote state exactly like a CI job; its source
commit must be pushed and reachable per [Pinned commits](#pinned-commits). CI
later continues from the state those runs recorded.

`--local` runs orchestration against local-only state, for environments
without a remote and for scratch experiments:

- `nyl state init -e <env> --local` starts local state from scratch at local
  refs `refs/nyl/local/<env>/desired` and `refs/nyl/local/<env>/observed`,
  for an environment with no remote state.
- Otherwise the first local run for an environment copies the remote state refs
  to those local refs; later local runs continue from them. `--local --reset`
  copies again.
- Local runs write transition commits and run checkpoints only to local refs,
  with a `Nyl-Local: true` trailer, and never push state.
  `nyl status -e <env> --local` shows the local view.
- A local run takes the remote lease (`nyl/<env>/lease`), because the lease is
  coordination, not state: a local run and a CI run of the same environment
  never execute at the same time. This needs push access to the lease ref.
- `--local-lease` explicitly opts into a local lease instead, for working
  without that access, and accepts that CI may run at the same time. Local
  state without a remote always uses a local lease.
- Effects are real, and local state is never pushed, so remote runs do not
  notice them: a unit whose remote receipt is current is not executed again,
  and the remote state keeps describing what ran before. A local run next to
  remote state warns about every unit it executes and names the remote
  command that records it again, such as `nyl reconcile -e <env> --unit
  <u>` after the change is committed. Units whose recovery policy is not
  `converge` run locally only when named with `--approve`.

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
  GitRepository's `publishURL`. Today `publish-tree` authenticates through the
  SSH agent only; SSH keys and HTTPS tokens for both are an M3 item.
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
    AwaitingApproval { digest: ChangeDigest },               // plan changed since approval; no effects
    Failed { category: FailureCategory, retryable: bool },
    Uncertain,
}
```

- `ExecutionContext` provides the worktree at the effective source revision,
  the resolved spec, admitted environment variables and secrets, the approved
  change digest for `bind: plan` units, the deadline, a cancellation signal,
  the unit's observed file, and a transcript sink that masks secret values.
- Every execution gets its own worktree and its own temporary directory for
  tool state, such as `TF_DATA_DIR`, so parallel units sharing a `source.path`
  never share a working directory.
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
  failure; a missing or invalid outputs file, or a declared output that is
  missing or has the wrong type, also fails the execution. Undeclared outputs
  are ignored with a warning. Failures are not retryable unless
  `idempotent: true`.
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
| `plan` | Resolve and run driver planning for ready units and for `deleting` units with teardown intent (destroy plans); report blocked units and incomplete plans | Nothing |
| `reconcile` | Resolve, then execute waves until nothing is ready | One transition commit per state ref |
| `status` | Report each unit's state from one snapshot of both refs | Nothing |
| `verify` | Run driver verification against current receipts | One observed commit with the latest observations |
| `recover` | Clear an uncertain condition or non-retryable failure for re-execution | One observed commit |
| `teardown` | Tear down a unit, replacing it if still selected; `--hold` keeps it down; `--all` tears down every unit; `--confirm-removed` ends a publication's teardown wait on the operator's word, under any strategy | One transition commit per state ref |
| `hold` | Freeze a unit: reconcile no changes to it until resumed | One desired commit |
| `resume` | Lift a hold | One desired commit |
| `state init` | Create state at the configured location; `--fresh` starts over deliberately; `--template` creates or updates an instance | `state.yaml` on each ref |
| `state delete` | Remove a template instance's state after all its units are torn down; `--teardown` tears them down first | One commit removing the instance's state |
| `state move` | Relocate state from another location and retire the old one | Moved history, plus a `state-moved` commit at the old location |
| `state forget` | Drop a `retained` tombstone or a `pending-teardown` unit without touching resources | One desired and one observed commit |
| `promote` | See the roadmap's promotion section | PromotionRecord |

All are top-level `nyl` commands. `release` is taken by Kubernetes release
history and `delete` by source editing, so lifting a hold is `resume` and
tearing down is `teardown`.

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
- `--revision <commit>` reads state as of an earlier state commit; `--local`
  reads a local run's state. Output formats: `table` (default), `wide`,
  `yaml`, `json`, `name`.
- `get` only reads: it needs read access to the state repository and never
  takes the lease. Document and table forms exit 0 whenever they can read the
  requested state; value forms follow the rule above.
- `nyl create` and `nyl delete` stay source-only, for environments, units, and
  promotion paths. Artifacts and state records have no `create` or `delete`:
  drivers produce them, and state changes only through the operations above.
- `nyl status -e <env>` remains the environment overview: the run holding the
  lease and what it is executing, blockers, and suggested next actions. It
  keeps the exit categories below, so CI can check whether everything is
  reconciled.

Common options: `-e`/`--environment`, `--unit`/`--units`,
`--approve <unit>[=<digest>]`, `--approved-by`, `--approval-source`,
`--allow-teardown`, `--allow-incomplete`, `--confirm-removed`, `--renew`, `--no-extend`, `--template`, `--source`, `--local`,
`--concurrency`, and `--output json` for versioned machine results on
stdout, with human diagnostics on stderr.

Each unit has one state in `status`: `blocked` (with the unresolved reference or
non-current dependency), `ready`, `awaiting-approval`, `running` (with the run
holding the lease), `uncertain`, `failed`, `current`, `held`,
`pending-teardown`, `tearing-down`, or `retained`. Whether the latest receipt is current is
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
pipeline's `state init --template` would have extended the expiry first.

**Deletion.** `cache` and its only consumer `worker` are removed from source;
`cache` has the default `Teardown` policy, `worker` declares `Retain`. The next
run marks both `deleting`, replaces `worker`'s files with a `retained`
tombstone, and reports `cache` as `pending-teardown`, exiting 2. A run with
`--allow-teardown` tears `cache` down and removes its files. A new `cache`
added in the meantime waits for that and then receives a new uid. When
`worker` is restored unchanged, it adopts its tombstone and does not run.

## Remaining questions

| Question | Needed by |
| --- | --- |
| Approver lookup for CI systems other than GitHub | M3 |
| Creating and removing an inline target's catalog Application when no parent Application owns it | M5 |

Driver-specific questions are in the [infrastructure units contract](infrastructure-units.md).
