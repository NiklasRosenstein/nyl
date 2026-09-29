# Promotion and health evidence

**Status:** M1 contract for M5–M6. See [ROADMAP.md](../ROADMAP.md), the
[orchestration core contract](orchestration-core.md), which defines the state
promotion reads, and the [Release inputs contract](release-inputs.md), which
defines the bindings promotion feeds.

This contract defines PromotionPaths, PromotionRecords, how `nyl promote`
chooses what to promote, and the attestations and health observations that
evidence levels read. Promotion never observes anything itself and never
promotes native tool state: it moves a proven source commit and selected
values from one recorded state into another environment's desired state.

## Resources

### PromotionPath

`gitops.nyl/v1` `PromotionPath` has a static envelope, like an Environment.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `from` | `{environment}` or `{target}` | required | The source: a declared Environment, never a template instance, or a DeploymentTarget (see [Promotion sources](#promotion-sources)) |
| `to` | `{environment}` | required | The target environment, whose desired state holds the records |
| `evidence` | `published` \| `attested` | `published` | The level every covered unit and the source must reach (see [Promotion paths](#promotion-paths)) |
| `attestations` | `{units: {<unit>: [name]}, environment: [name]}` | the level's default | Replaces the required attestations per scope; with `published`, adds requirements |
| `maxAttestationAge` | duration | none | Older attestations do not count |
| `changeGate` | `none` \| `pullRequest` | `none` | `pullRequest` opens a reviewable change instead of writing the record |
| `verifyArtifacts` | boolean | `true` | Check that every promoted artifact still exists before recording |
| `coverage.applicationGroups` | list of names | every group | Narrows the Applications that count toward a source promotion's publication attestations; never what moves |
| `coverage.excludeApplications` | list of Application names | none | Applications knowingly accepted as unhealthy; they leave the aggregate for this path and are listed in each record |
| `coverage.requireApplications` | list of Application names | none | Applications that must meet the required attestations at whatever publication they run, without contributing values |
| `values` | map of value name to `{select}` | `{}` | The promoted values; value names are what `fromPromotion` bindings read |

A `select` names exactly one source field, with the same JSON Pointer
semantics ([RFC 6901](https://www.rfc-editor.org/rfc/rfc6901)) and resolver as
`fromUnit` references and `nyl get`:

| Selector | Reads |
| --- | --- |
| `{unit, input: <pointer>}` | The source unit's `resolvedSpec` in its desired document: what the source ran, such as `/source` or `/releases/platform~1web/image` |
| `{unit, output, pointer?}` | A declared, non-sensitive output of the source unit's receipt |
| `{unit, artifact, kind?, pointer}` | A field of an artifact the source unit's receipt lists, relative to the artifact's `spec` |
| `{input: <pointer>}` | For a target source only: a Release input of the target, under the same `/releases/<group>~1<release>/<input>` layout a publication unit's `resolvedSpec` uses |

Validation checks every path: the source and target exist, selected units are
selected by the source environment, pointers resolve against the source's
current documents or the kind's schema, and each value's type fits every
binding that reads it. `plan` reports the same findings for an existing path.

### PromotionRecord

`nyl promote` writes one `gitops.nyl/v1` `PromotionRecord` per path into the
target environment's desired state, `desired/promotions/<path>.yaml`, or, with
`record: source`, into the Environment's `source.fromPromotion.promoted` block
(see [Promoting an environment's source](#promoting-an-environments-source)).
Each promotion replaces the path's record; Git history keeps earlier ones.

| Field | Meaning |
| --- | --- |
| `path` | The PromotionPath that wrote it |
| `sequence` | Integer that increases with every promotion into the target environment, on any path; "newest" always means the highest `sequence` |
| `from` | The source state: `{environment, run, desiredCommit, observedCommit}`, or for a target source `{target, repository, branch, commit}` |
| `sourceCommit` | The source commit of that state, present when the path promotes the target's source or a value's lineage includes one |
| `values.<name>` | `{value, select, unit: {name, uid}, executionKey or inputDigest, state}`; `state` is present when a value-only path took the value from another state than `from` |
| `evidence` | `{level, attestations: [{scope, name, result, at}], observations: […], override: {reason} \| null}` |
| `ignoredAttestations` | `[{scope, name, reason, by}]` from `--ignore-attestation` |
| `excludedApplications` | The path's `coverage.excludeApplications` at promotion time |
| `superseded` | `[{repository, commit, reason, by}]` from `--supersede` |
| `artifactsVerified` | Whether the artifact check ran |
| `requested` | `{by, identity, source, at, reason}`, recorded like an approval |

Resolution of `fromPromotion` reads only records. `desired/environment.yaml`
records the highest `sequence` a run applied, so a run refuses an older
`record: source` block, as described below.

### Exit categories

`nyl promote` uses the core contract's exit categories:

| Exit | When |
| --- | --- |
| 0 | The record was written, or the pull request opened or updated |
| 1 | Configuration error: an invalid path, a broken selector, an unreachable commit, a missing artifact |
| 2 | Nothing was written because evidence is missing, stale, mixed, or failing (`NYL-PROMOTE-NO-EVIDENCE`), a newer promotion on another path is not contained (`NYL-PROMOTE-SUPERSEDES`), or the target's lease is held |

`nyl promote` takes the target environment's lease, because it writes the
target's desired state; it only reads the source's state, and never takes the
source's lease. `--dry-run` takes no lease and writes nothing, and exits as the
promotion would.

The [stale source evidence walkthrough](orchestration-core.md#walkthroughs)
is an executable scenario,
[`stale-promotion-evidence`](../nyl/tests/scenarios/walkthroughs/stale-promotion-evidence/scenario.yaml).

## Promotion paths

A PromotionPath is the explicit link between source selectors and target
bindings. Source and target may differ in unit names, input names, and document
shape; the path states the mapping. [Resources](#resources) lists every field.

```yaml
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-staging}
spec:
  from: {environment: dev}
  to: {environment: staging}
  evidence: attested           # published | attested
  changeGate: pullRequest      # or: none
  values:
    webImage:
      select: {unit: kubernetes, input: /releases/platform~1web/image}
    databaseSource:
      select: {unit: database, input: /source}
```

Target consumers name the value and, optionally, the path, never the source unit
(see [Several paths into one environment](#several-paths-into-one-environment)
for the path-less form):

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
  - `attested`: published, and every attestation the covered units and the
    source environment declare passes for that state (see
    [Attestations](#attestations)). A unit that declares none satisfies it with its
    current receipt.

  A path's `attestations` field adjusts the required set per unit and for the
  environment; each entry replaces that scope's default, and scopes it does
  not name keep the default of the level:

  ```yaml
  spec:
    evidence: attested
    attestations:
      units:
        kubernetes: [accepted]   # from this unit, accepted is enough
        database: []             # receipt only, even if it declares healthy
      environment: [qa]          # default: all the environment declares
  ```

  With `evidence: published`, the same field adds requirements, so
  `attestations: {units: {kubernetes: [accepted]}}` means receipts plus
  `kubernetes`'s `accepted`. Validation rejects a unit the path does not cover
  and a name its scope does not declare. Drift verification writes no receipt
  and attests nothing.
- A failing attestation for a state blocks its promotion on every path,
  whether or not the path requires it. A later passing attestation of the same
  name clears it; `nyl promote --ignore-attestation <unit>/<name>=<reason>`
  (or `environment/<name>`) overrides it, and the PromotionRecord keeps the
  reason.
- One rule selects values: promote, per value, the newest source state whose
  evidence proves the required level. Every attestation names the state it
  attests; a KubernetesPublication's `accepted` and `healthy` also name, per
  Application, the publication it runs, so at `attested` each value comes from
  what its consumer ran when it was attested.
- `nyl promote` never observes anything itself: it reads only attestations
  recorded beforehand, so it runs from a workstation without cluster
  credentials. Drivers record them during `reconcile` or `nyl verify -e
  <env>`, typically in CI where Argo CD access exists, and `nyl attest`
  records them from people and external systems. A path may set
  `maxAttestationAge`, such as `1h`, beyond which a recorded attestation no
  longer counts. The attestations used are stored in the PromotionRecord.
- Which states the values come from depends on the path:
  - A path that promotes the target's source takes the source commit and every
    value from one source state (see
    [Promoting an environment's source](#promoting-an-environments-source)). At
    `published`, that is the newest state at which every selected unit has a
    matching receipt or, for a target source, the newest publication commit;
    at `attested`, the state the source ran when it was attested. Mixed
    revisions block.
  - A path that promotes only values takes each value from the state its own
    consuming Application or unit runs, which may differ between values when
    manual syncs left Applications at different revisions. Each value is then
    one its consumer really ran; the PromotionRecord lists each value's state.
  - `--state-revision <desired commit>` selects an exact state and
    `--revision <source commit>` the newest with that source commit, for all
    values of the promotion. Promoting an older state than the evidence level
    would select requires `--evidence published`, recorded as an override,
    for example to roll back.
- `nyl promote dev-to-staging [--value …]` writes a
  PromotionRecord into the target environment's desired state: per value, its
  selector, the source unit's identity and incarnation, the source revision it
  came from, and its receipt or input digest; plus the attestations that proved
  the set at `attested`. With `changeGate: pullRequest` it opens a
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

### Promoting an environment's source

An environment whose `source` is
`fromPromotion` runs its definitions at a source commit its promotion path's
source proved, so a change to what the code does reaches it only through
promotion. Values then carry only results that must not be rebuilt, such as
image digests. A `revision` with a `commit` lock is an ordinary lock that
`nyl update source-locks` moves; it is never a promotion target and cannot
bind `fromPromotion`.

```yaml
# config/environments/prod.yaml
spec:
  source:
    fromPromotion:
      path: dev-to-prod
      record: state             # default; or `source`, see below
---
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-prod}
spec:
  from: {environment: dev}
  to: {environment: prod}
  evidence: attested
  values:
    webImage:
      select: {unit: web-image, artifact: image, pointer: /reference}
```

- The unit of promotion is one recorded state of the path's source: a
  transition commit pair, pushed together with one `Nyl-Run-Id`, that ties
  together the source commit, the desired documents, and the receipts with
  their artifacts. The PromotionRecord records that state (`from`: the source
  environment, run, and desired and observed commits, or a target source's
  publication commit), its source commit, and every value read from it, so
  the promoted source and the promoted values always come from the same run.
  Chains compose: a QA environment promoted from dev is itself a source whose
  states a `qa-to-prod` path promotes.
- Choosing the state:
  - Without flags, at `published`, the newest state in which every unit the
    target also selects has a current receipt.
  - Without flags, at `attested`, the newest state whose required
    attestations all passed; for publication units, what the newest recorded
    attestations show running, and mixed revisions block.
  - `--revision <source commit>` takes the newest qualifying state with that
    source commit, and `--state-revision <desired commit>` takes exactly one.
    An explicitly chosen state satisfies `attested` through attestations
    recorded for that state, such as those the publication unit's observe mode
    wrote while it ran; the record names them, with their times. Only a state
    without them needs `--evidence published`, recorded as an override.
  - Rolling back is promoting an older state:
    `nyl promote dev-to-prod --revision S1 --reason "…"`. The target's own
    units then run at the older source commit, and their plans and approvals
    show what that changes. The supersede check below compares only against
    other paths, so a rollback on the same path never needs `--supersede`.
- Before recording, `nyl promote` checks that every promoted artifact still
  exists, for images with `docker buildx imagetools inspect`, and refuses if
  one is gone. A path's `verifyArtifacts: false` or the invocation's
  `--no-verify-artifacts` skips the check, for runners without registry
  access; the record then says `artifactsVerified: false`.
- Inspection: `nyl get states -e <env>` lists an environment's recorded states
  with source commit, results, publication, and recorded attestations;
  `nyl get promotion-candidates <path>` evaluates the path's source states
  against the path, showing the level each reaches or why not, the values it
  would carry, and which one the target runs now; `nyl promote … --dry-run`
  shows the chosen state and the exact change without writing anything.
- Evidence covers the whole source, not only the selected values: in that
  state, every unit the target environment also selects has a current
  receipt, and at `attested` every such unit's required attestations pass for
  that state. A publication unit's `accepted` and `healthy` aggregate all
  Applications it generates: `accepted` when all of them run that
  publication, `healthy` when all are also Healthy. Mixed revisions block,
  because one source commit must fit every value. A path's `coverage.excludeApplications` names
  Applications it knowingly accepts as unhealthy; they leave the aggregate for
  that path and are listed in the PromotionRecord. A publication unit without
  observe mode declares no attestations and is satisfied by its receipt.
- `record` decides where the PromotionRecord lives. With `record: state`, the
  default, it is written into the target's desired state, as a pull request
  against the desired ref under `changeGate: pullRequest`. With
  `record: source`, `nyl promote` writes it into the Environment's own
  `source` block, as a pull request against source under
  `changeGate: pullRequest`:

  ```yaml
  source:
    fromPromotion:
      path: dev-to-prod
      record: source
      promoted:                        # written by nyl promote, never by hand
        sequence: 7                    # increases with every promotion into this environment
        path: dev-to-prod              # the path that supplied it, when `paths` lists several
        sourceCommit: 3e7b9c…
        from: {environment: dev, run: 0b8f6c1e-…, desiredCommit: 77aa…, observedCommit: 41f0…}
        values: {webImage: registry.example.com/web@sha256:4f0c…}
        evidence: {level: attested, attestations: [kubernetes/accepted, kubernetes/healthy], at: 2026-09-25T16:40:00Z}
        artifactsVerified: true
        superseded: []                 # commits replaced with --supersede, with reasons
  ```

  The source commit and its values are then one reviewed change. The block is part of `source`, so it is read from the
  entry worktree like the rest of that field, and validation rejects a block
  that does not match the source state it names. `source-locks` never touches
  it.
  - The block carries `sequence`, which `nyl promote` increments with every
    promotion into the environment, and state records the highest sequence a
    run applied. A run refuses a block with a lower sequence, such as one from
    a re-run of an old CI job, and names `nyl promote --revision` as the way
    to roll back, so an old checkout never rolls prod back unreviewed.
    Reverting the promotion commit in source is refused the same way; the
    rollback is a new promotion with a higher sequence.
- Either way, the source commit and the values sit in one record, so they
  cannot diverge. An environment that follows its entry worktree, or a
  revision, is not a promotion target for its source.
- Before the first promotion, an environment with a `fromPromotion` source has
  no source commit, and `reconcile` exits 2 naming `nyl promote`.
- The target may still select a unit dev also runs, such as `web-image`, and
  rebuild it at the promoted commit instead of binding dev's result. The
  PromotionRecord then says the result was rebuilt rather than claiming dev's
  evidence for it, image tags include the environment, and Nyl warns only
  when an environment builds a unit whose artifacts its `fromPromotion`
  bindings replace.
- A target source (`from: {target: …}`) promotes the source commit its
  publication recorded, under the same rules.
- Prod-only changes, such as a replica count, reach prod with the next
  promotion of a commit that contains them. Hotfixes that cannot wait for dev
  take a path of their own (see [Hotfixes](#hotfixes)).
- A path that promotes a source may narrow the Applications that count toward
  its publication units' `accepted` and `healthy` to named ApplicationGroups,
  `coverage: {applicationGroups: [web]}`. It never narrows what moves; the
  promoted commit still carries every group's definitions.

### Several paths into one environment

An environment may be the target of several paths. `source.fromPromotion`
takes `path` for one path or `paths` for several; the source commit comes from
the newest PromotionRecord on those paths, and desired state records which
path supplied it.

A `fromPromotion` binding names a `value` and, optionally, a `path`:

- Without `path`, it reads the newest record into this environment that
  carries the value. In an environment whose source is promoted, only records
  on its source paths count, so the source commit and the values proven with
  it always move together.
- With `path`, it reads only that path, for values with a lineage of their
  own, such as a vendor image promoted independently of the source.
- Validation rejects a binding that no path into the environment can supply,
  naming the value and the paths it checked; every path in `paths` must define
  the values that path-less bindings read. `nyl get promotions -e <env>` shows
  the supplying path and record for every binding, and `plan` reports when
  that path changes.

Newest wins at resolution, which only reads records. What a path may record is
decided when `nyl promote` writes, by the supersede check:

- **Rule.** Before writing a value on path P, `nyl promote` requires the
  value's source commit to contain the source commit of every record for that
  value that another path wrote since P last promoted it. A commit contains
  another when that commit is its ancestor, or when a commit with the same
  changes (`git patch-id`, as `git cherry` uses) is, so clean cherry-picks and
  single-commit squash merges pass.
- **Source commit of a value.** A promoted source commit, such as an
  environment's source or a unit's `webSource`, is its own. An artifact or
  output has the source commits that its producing receipt's desired unit
  resolved, such as an OciImage's Git contexts, so an image built before a
  hotfix cannot replace the hotfixed image even on a path that promotes only
  digests. Commits are compared within their own repository. A value without a
  known source commit, such as an image from outside Nyl or a command output,
  is listed as unchecked by `nyl promote` and `plan`. A subset promotion checks
  only the values it writes.
- **Refusal.** A refused promotion writes nothing and exits 2 with
  `NYL-PROMOTE-SUPERSEDES`, because nothing failed: the promote job succeeds
  once the missing commits are merged. The message lists the missing commits
  per repository and a range to copy: "`S5` is missing 2 commits from
  `hotfix-to-prod`: `S1..c3d4` (a1b2 fix pool size, c3d4 bump timeout); merge
  them or pass `--supersede S1..c3d4=<reason>`".
- **Override.** `--supersede <commit or range>[=<reason>]` accepts that the
  promotion replaces those commits, because the change was made differently
  or is dropped on purpose. It is repeatable; an entry without its own reason
  takes `--reason`, and an entry with neither is rejected. Ranges are Git
  ranges resolved at promotion; commits in them that are not missing are
  ignored, and a missing commit no entry covers keeps the promotion refused.
  When the refusal involves several repositories, an entry is qualified with
  the repository, `--supersede web:S1..c3d4=<reason>`, by GitRepository name
  or, for an inline repository, the name the refusal prints; an unqualified
  full commit ID is accepted when it exists in only one of them.
- **Record.** The PromotionRecord lists every superseded commit with its
  repository URL, its reason, and who gave it, never the range, and a pull
  request under `changeGate: pullRequest` shows them. A later promotion on P
  is no longer checked against them, because P has now promoted after them.

### Hotfixes

A promoted environment runs only what its source proved, so a fix that cannot
wait for dev's unreleased work takes a path of its own:

```yaml
# prod
spec:
  source:
    fromPromotion: {paths: [dev-to-prod, hotfix-to-prod]}
---
apiVersion: gitops.nyl/v1
kind: Environment
metadata: {name: prod-hotfix}
spec:
  source: {revision: release/prod}     # cut from prod's current source commit
  unitSelector: {matchLabels: {prod: 'true'}}
---
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: hotfix-to-prod}
spec:
  from: {environment: prod-hotfix}
  to: {environment: prod}
  evidence: attested
  values:
    webImage: {select: {unit: web-image, artifact: image, pointer: /reference}}
```

- `prod-hotfix` follows the `release/prod` branch, not prod: nothing in Nyl
  points from it back to prod, so the only edge is `hotfix-to-prod` into prod.
  It builds and runs the fix, typically on a small target of its own, because
  it cannot publish into prod's target, and can stay idle between hotfixes.
  `hotfix-to-prod` promotes it with the same evidence as the regular path, and
  prod's own units keep their approvals, `requireDigest` included. Bindings do not change when the path is
  added, because path-less bindings follow whichever record supplied the
  source. Prod lists `release/*` in `protectedRefs`, and CI reconciles
  `prod-hotfix` on pushes to `release/prod`.
- A hotfix runs like this:

  | Step | Branches | Nyl |
  | --- | --- | --- |
  | prod runs S1 | `main` has moved on to S5 | `nyl get environments prod` shows source commit S1 |
  | Start | `git switch -c release/prod S1`, commit fix `a1b2`, push | — |
  | Try | — | CI: `reconcile -e prod-hotfix` builds from `a1b2` and deploys to the hotfix target |
  | Ship | — | `promote hotfix-to-prod`, then `reconcile -e prod`: prod runs `a1b2` with the hotfix image |
  | Merge back | a pull request merges `release/prod` into `main` → S6 | the next `promote dev-to-prod` passes the supersede check |
  | Clean up | keep `release/prod` until prod runs a commit reachable from `main` | — |

  Prod's source commit must stay fetchable for re-execution and teardown; a
  merge commit makes `a1b2` reachable from `main`, while a cherry-picked fix
  leaves it reachable only from `release/prod`. The next hotfix re-creates the
  branch from what prod runs by then.
- A lighter variant selects only `web-image` in `prod-hotfix` and gives it no
  target: it builds the fix and nothing runs it, so `hotfix-to-prod` uses
  `evidence: published`, the promoted digest is all it proves, and prod's
  reviewed-plan approvals carry the rest.
- The next `dev-to-prod` promotion is refused until `main` contains the fix,
  typically by merging `release/prod` into `main`, or until `--supersede`
  names it. The rule is symmetric: a hotfix branch cut before a later dev
  promotion is refused too, and is cut again from what prod runs.
- A promotion of a commit that no environment ran is deliberately not
  offered: the hotfix environment gives the fix the same evidence as any other
  change.

### Splitting environments by release cadence

A source promotion moves
everything in one environment together, and health is aggregated per
publication unit, which is one target. Parts that should move and gate
independently therefore belong in separate environments, which may share a
cluster, a deploy branch, and a state ref:

- A cluster's platform components (ingress, autoscaling, observability, Argo CD
  itself) in a `production-infra` environment with its own target, prefix,
  and ApplicationGroups, promoted from `staging-infra`; the services built from
  source in `production-services`, promoted from `staging-services` with their
  image digests. Each gates on its own target's health, and services read the
  infrastructure environment's outputs through cross-environment references.
- The same shape without anything built from source: third-party charts on a
  dev and a prod cluster, split into `core` and `apps` environments so a chart
  upgrade proven in dev can reach prod without waiting for unrelated changes.
- A production environment that follows a release branch instead of a
  promotion, such as a `main` that receives merges from `develop` and hotfixes
  of its own, is a `revision` environment: it builds its images from that
  branch and reuses no digests from another environment, because the merged
  branch can differ from anything the earlier environment ran.

Environments split this way are still reconciled together when that is
convenient: `nyl reconcile` accepts repeated `-e`, a label selector with
`-l`, or an EnvironmentGroup with `-g`, and runs producers before consumers,
each with its own lease and transition commit. A group can also carry defaults
its members share, such as common values and the state repository.

### Promotion sources

`from` selects either an environment, as above, or a
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
      select: {input: /releases/platform~1web/image}   # /releases/<group>~1<release>/<input>
```

- A target's publication commits are the commits that change
  `<prefix>/_nyl/index.json`. Commits by other targets on a shared branch and
  external state write-backs are never value sources.
- At `published`, a target source reads one publication commit: the newest by
  default, or an exact one with `--from-revision`. That commit already contains
  the manifests rendered from the values. At `attested`, each value comes from
  the publication its consuming Application runs, as described under health
  evidence.
- The ownership index stores input digests, not values. Promotion recovers each
  value from its recorded provenance: the carried or base-commit state file, the
  locked `fromGit` blob, or the binding at the recorded source commit. It then
  verifies the value against the recorded `@input` digest. A publication made
  from a dirty source worktree is not a promotion source.
- A non-orchestrated target records its attestations on its own publication
  branch (see [Kubernetes publications](#kubernetes-publications)), so a target source reaches `attested` from
  recorded evidence like an environment source. `--observe` observes Argo CD
  directly instead, with credentials for its cluster.
- The PromotionRecord adds, per value, the source target, publication
  repository, branch, and commit, and the input digest to its lineage, plus the
  observation that proved the set.
- `to` is always an environment, because the PromotionRecord lives in that
  environment's desired state.

### Locked state without orchestration

A target that belongs to no environment can still read another target's
published state through a locked `fromGit` binding. `nyl update source-locks
--target production` moves the lock to the head of the branch, and the pull
request that commits it is the review. The lock has no health gate and never
selects what the source target runs; that selection, its evidence, and its
record belong to a PromotionPath:

| | Locked `fromGit` (M2) | PromotionPath (M6) |
| --- | --- | --- |
| Target binding | `fromGit` at a commit of the source branch | `fromPromotion` naming a value, optionally its path |
| Moves with | `nyl update source-locks --target …`, then a pull request | `nyl promote <path>` |
| Selects | The branch head | The publication each consuming Application runs, or an exact one |
| Health gate | None | `evidence: attested` on the path |
| Record | The lock in source | A PromotionRecord in target desired state |

## Health evidence

### Attestations

Evidence beyond a receipt is an attestation: a named pass or
fail result about one unit's current receipt, or about an environment's
recorded state. It is recorded at `observed/units/<unit>/attestations/<name>.yaml`
or `observed/attestations/<name>.yaml` with its result, time, details, and
source. The unit's driver, not Nyl's core, knows how its health is observed,
so every source writes the same record:

| Source | When | Examples |
| --- | --- | --- |
| The driver, during `reconcile` | The apply itself proves it | A Terraform apply whose `check` blocks or health waits pass; a command unit that reports its result |
| The driver, during `nyl verify` | Observed afterwards | A KubernetesPublication's `accepted` and `healthy` from Argo CD; configured probes for a unit such as an OpenTofu-managed ECS service |
| External, through `nyl attest` | Someone or something else knows | A monitoring hook, a manual QA pass, an end-to-end job |

- **Declared names.** A driver reports, from a unit's spec, which attestations
  it can produce: a KubernetesPublication in observe mode declares `accepted`
  and `healthy`; an OpenTofu unit declares `healthy` once its spec configures
  apply-time checks or probes, and nothing otherwise. A unit's
  `spec.attestations` adds names only external sources supply, such as
  `[{name: healthy, from: external}]` for a service nothing in Nyl can
  observe, and `Environment.spec.attestations: [qa]` declares environment-wide
  ones. No name is reserved; `healthy` is a convention of the drivers.
- **`nyl attest`.** `nyl attest -e <env> [--unit <u>] --name <n> --result
  pass|fail [--reason …] [--url …]` records an attestation for the unit's
  current receipt, or without `--unit` for the environment's current state;
  `--state-revision` or `--revision` attests an earlier state. It refuses names
  the scope does not declare, so a typo cannot create evidence nobody
  requires. The attester is recorded like an approver, through the same
  identity and approval sources. `nyl attest --target <name>` writes to a
  non-orchestrated target's `_nyl/observations/attestations/`.
- **Lifetime.** An attestation belongs to the receipt or state it names; a new
  receipt starts with its declared attestations pending. The newest
  attestation per name and receipt wins, and history keeps earlier ones.
- **Use.** Promotion requires them through `evidence: attested` and a path's
  `attestations` (see [Promotion paths](#promotion-paths)), and a failing one blocks promotion of
  its state on every path. Dependents require them with `dependsOn` or
  `fromUnit` (see the orchestration core contract). `nyl get states` and
  `nyl get promotion-candidates` show them per state, and `status` shows
  failing ones.

### Kubernetes publications

A KubernetesPublication attests `accepted` and
`healthy` by observing Argo CD. Nyl knows the Applications it generates for a
target, their ArgoCDInstance, and its Cluster. A mode that applies manifests
directly and attests `healthy` from rollout status during `reconcile` fits the
same model and is not in the initial scope.

- **Observer.** Nyl reads the target's generated Applications from the Argo CD
  control-plane Cluster through its local context. Health checks need
  credentials for that Cluster, which publication does not, so observations
  are recorded where those credentials exist, typically in CI, and decisions
  read the recorded evidence.
- **Where observations are recorded.** An orchestrated environment's
  publication units record into its observed state. A target without an
  environment records into its own publication branch:
  `nyl verify --target <name>` writes `<prefix>/_nyl/observations/health.yaml`
  with each Application's running revision, matched publication, and health,
  and the target's `accepted` and `healthy` under `_nyl/observations/attestations/`.
  - It commits only when that content changes or the previous observation is
    older than a refresh interval, which keeps `maxAttestationAge` satisfiable
    without a commit per run.
  - `_nyl/observations/` is a reserved path: never rendered, never removed by a
    publish, and neither owned nor unowned for reconciliation; a teardown of
    the target removes it. No Argo CD Application sources it, and verify and
    publish commits touch disjoint files, so the branch's retry rule covers
    concurrent writes.
  - The branch history keeps older observations, so an older publication can
    be promoted or locked from recorded evidence.
  - This adds no refs: observations live with the tree they describe.
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
- **Coverage.** For a path that promotes only values, the Applications whose
  Releases produced the promoted values must meet the required `accepted` or
  `healthy`, and a PromotionPath's `coverage.requireApplications` adds
  Applications that must be healthy at whatever publication they run, without
  contributing values. For a path that promotes
  the target's source, coverage is every Application of every publication unit
  the target also selects, aggregated per unit as described above.
- **Where Application data lives.** Only a `KubernetesPublication` unit knows
  Applications, because it generated them. Its observation records every
  Application's running revision and health, and the unit's `accepted` and
  `healthy` attestations aggregate it for the matched publication; promotion
  decisions use the attestations, and the per-Application list explains why a
  unit fell short.
- **Manual syncs.** For a path that promotes only values, Applications synced
  to different publications do not block promotion. Promotion blocks only when
  a covered Application runs a revision that matches no source publication, or
  does not meet the required attestation. A path that promotes the target's source also
  blocks on mixed revisions.
- **Decision evidence is always recorded.** The observation behind every
  promotion (time, Application, running revision, health) is stored in the
  PromotionRecord; observations themselves are recorded in observed state or on the target's
  publication branch.
- **Observation history.** One observation proves what runs now. Promoting a
  commit that no longer runs, requiring a minimum healthy duration, and
  guarding against Applications that flap between healthy and unhealthy need
  observations over time; Argo CD does not reliably report how long an
  Application has been healthy. The publication unit's observe mode records
  them in an environment's observed state; periodic observation is part of
  continuous operation.
- **Meaning.** Argo CD health means Kubernetes considers the resources ready,
  such as a completed Deployment rollout. It does not prove the application
  works. Application-level checks, such as HTTP probes, smoke tests, or manual
  QA, are further attestations: from a command unit, a driver's probes, or
  `nyl attest`.

