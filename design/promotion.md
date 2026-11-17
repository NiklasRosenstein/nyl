# Promotion and health evidence

**Status:** M1 contract for M5–M6 and M11, with the decisions of the
[M6 plan](m6-plan.md#open-decisions) folded in. M6 implements promotion
between environments; DeploymentTargets as promotion sources and targets,
`nyl attest --target`, `changeGate: pullRequest`, and attesting an earlier
state follow in M11, and until then validation rejects them with
`NYL-PROMOTE-UNSUPPORTED`, naming M11. See [ROADMAP.md](../ROADMAP.md), the
[orchestration core contract](orchestration-core.md), which defines the state
promotion reads, and the [Release inputs contract](release-inputs.md), which
defines the bindings promotion feeds.

This contract defines PromotionPaths, PromotionRecords, how `nyl promote`
chooses what to promote, and the attestations that evidence levels read. Promotion never observes anything itself and never
promotes native tool state: it moves a proven source commit and selected
values from one recorded state into another environment's desired state.

## Resources

### PromotionPath

`gitops.nyl/v1` `PromotionPath` has a static envelope, like an Environment.

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `from` | `{environment}` or `{target}` | required | The source: a declared Environment, never a template instance, or (M11) a DeploymentTarget (see [Promotion sources](#promotion-sources)) |
| `to` | `{environment}` or `{target}` | required | The target environment, whose desired state holds the records, or (M11) a DeploymentTarget outside every environment, whose source holds them (see [Promoting into a target](#promoting-into-a-target)) |
| `evidence` | `published` \| `attested` | `published` | The level every covered unit and the source must reach (see [Promotion paths](#promotion-paths)) |
| `attestations` | `{units: {<unit>: [name]}, environment: [name]}` | the level's default | Replaces the required attestations per scope; with `published`, adds requirements |
| `maxAttestationAge` | duration | none | Older attestations do not count |
| `changeGate` | `none` \| `pullRequest` | `none` | `pullRequest` (M11) opens a reviewable change instead of writing the record |
| `verifyArtifacts` | boolean | `true` | Check that every promoted artifact still exists before recording |
| `coverage` | map of unit name to `{groups, exclude, require}` | `{}` | Per unit that runs as several deployments, which of them count toward its running-scoped attestations for this path (see [Coverage](#kubernetes-publications)); names are those the unit's driver lists for it |
| `coverage.<unit>.groups` | list of group names | every group | Narrows the deployments that count toward a source promotion to those of these groups; never what moves. Refused on a path that promotes only values |
| `coverage.<unit>.exclude` | list of deployment names | none | Deployments knowingly accepted as falling short; they leave the unit's aggregate for this path and are listed in each record |
| `coverage.<unit>.require` | list of deployment names | none | Deployments that must meet the required attestations at whatever publication they run, without contributing values |
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

There is no selector for an [observed value](orchestration-core.md#observed-values)
yet. An `input` selector reads what the source unit ran, including an input it
resolved from an observed value, like one resolved from an output.

Validation checks every path: the source and target exist, selected units are
selected by the source environment, pointers resolve against the source's
current documents or the kind's schema, and each value's type fits every
binding that reads it. `plan` reports the same findings for an existing path.

Each failure has a stable code and a fix: `NYL-PROMOTE-ENVIRONMENT-UNKNOWN` (the
source or target is not a declared Environment, an instance of a template
included), `NYL-PROMOTE-SELECTOR-UNIT` (a selector or an `attestations` scope
names a unit the source does not select, or the path does not cover),
`NYL-PROMOTE-SELECTOR-INVALID` (a pointer that resolves neither in the unit's
rendered spec nor in its kind's schema, an undeclared or `sensitive` output, or a
malformed selector), `NYL-PROMOTE-VALUE-TYPE` (a value's type does not fit a unit
field that reads it; a Release input's declared type is not compared yet),
`NYL-PROMOTE-BINDING-UNKNOWN` (a `fromPromotion` binding names a path or value no
path declares), `NYL-ATTESTATION-UNDECLARED` (a required attestation its scope
does not declare), `NYL-PROMOTE-COVERAGE-UNKNOWN` (`coverage` names a unit
the source does not declare, select, or enable, a unit whose kind runs no
deployments, or a group or deployment the unit's driver does not list for it),
`NYL-PROMOTE-COVERAGE-INEFFECTIVE` (a path that promotes only values sets a
unit's `groups`, or the unit's kind has no running-scoped attestations for its
spec, so its entry selects nothing), `NYL-PROMOTE-COVERAGE-UNCHECKED` (the
unit's deployments could not be listed, such as when its target does not
render; an error in `nyl validate`, a warning in the commands that work on one
environment, because they judge another environment only by what they need),
`NYL-UNIT-KIND-UNKNOWN` for a coverage unit whose kind no driver serves, and
`NYL-PROMOTE-UNSUPPORTED`. A pointer into an optional
field the rendered spec omits resolves through the kind's schema.

### PromotionRecord

`nyl promote` writes one `gitops.nyl/v1` `PromotionRecord` per path into the
target environment's desired state, `desired/promotions/<path>.yaml`, or, with
`record: source`, into the Environment's `source.fromPromotion.promoted` block
(see [Promoting an environment's source](#promoting-an-environments-source)).
Each promotion replaces the path's record; Git history keeps earlier ones.
The record has one shape wherever it is kept (`apiVersion` and `kind` are
written in the state file and left out of a block that sits in a resource): in desired state, in an
Environment's `promoted` block, and (M11) in a target's `spec.promotions`. The
shape already holds M11's target source (`from: {target, repository, branch,
commit}`, and a value without a source `unit`), so M11 changes no format; M6 never
writes it, and a reader loads it.

| Field | Meaning |
| --- | --- |
| `path` | The PromotionPath that wrote it |
| `sequence` | Integer that increases with every promotion into the target environment, on any path; "newest" always means the highest `sequence` |
| `from` | The source state: `{environment, run, desiredCommit, observedCommit}`, or for a target source `{target, repository, branch, commit}`. For a path that promotes only values, the newest state any value came from |
| `sourceCommit` | The source commit of that state, present when the path promotes the target's source or a value's lineage includes one |
| `values.<name>` | `{value, select, unit: {name, uid}, executionKey or inputDigest, state}`; `inputDigest` is the `sha256:` digest of the canonical JSON of the value an `input` selector read; `state` (`{run, desiredCommit, observedCommit}`) is present when a value-only path took the value from another state than `from` |
| `evidence` | `{level, attestations: [{scope, name, result, at, details}], override: {reason} \| null}`; `details` are the driver's, such as each deployment's running publication and health |
| `ignoredAttestations` | `[{scope, name, reason, by}]` from `--ignore-attestation` |
| `excludedDeployments` | `{<unit>: [name]}`: the deployments the path's `coverage` excluded at promotion time, by unit; units and names sorted, each name once, a unit that excludes none left out, and `{}` when nothing was excluded |
| `superseded` | `[{repository, commit, reason, by}]` from `--supersede` |
| `artifactsVerified` | Whether the artifact check ran |
| `requested` | `{by, identity, source, at, reason}`, recorded like an approval |

Resolution of `fromPromotion` reads only records. `desired/environment.yaml`
records the highest `sequence` a run applied, so a run refuses an older
`record: source` block, as described below.

`desired/promotions/<path>.yaml` is a file the state layout of format `v1`
always listed, added without a version change. A Nyl older than the release
that writes it keeps the file through its transitions but cannot act on it:
it refuses an environment whose source is `fromPromotion`, and must not run
`reconcile` in an environment whose bindings read promotions. Its
`state move`, when it copies documents rather than history, does not carry
the file, and its `state restore` leaves the file as it is.

### Exit categories

`nyl promote` uses the core contract's exit categories:

| Exit | When |
| --- | --- |
| 0 | The record was written, or (M11) the pull request opened or updated; or the path's record already names the chosen state and values and supplies every value of the target, so nothing was written (`outcome: unchanged`) |
| 1 | Configuration error: an invalid path, a broken selector, an unreachable commit, a missing artifact (`NYL-PROMOTE-ARTIFACT-MISSING`), a `--supersede` entry with neither its own reason nor `--reason` (`NYL-PROMOTE-SUPERSEDE-REASON`), a form M6 does not support yet (`NYL-PROMOTE-UNSUPPORTED`), a shallow entry clone the supersede check cannot use (`NYL-PROMOTE-SHALLOW-CLONE`), or a `record: source` block that cannot be committed or pushed (`NYL-PROMOTE-SOURCE-UNWRITABLE`) |
| 2 | Nothing was written because evidence is missing, stale, mixed, or failing (`NYL-PROMOTE-NO-EVIDENCE`), a newer promotion on another path is not contained (`NYL-PROMOTE-SUPERSEDES`), the target's lease is held, the push of a `record: source` commit was refused because the branch moved (`NYL-PROMOTE-SOURCE-REJECTED`), or that push failed without a refusal and may have reached the remote, so no marker was written (`NYL-PROMOTE-SOURCE-UNCERTAIN`) |

Other codes of the promotion rules: `NYL-PROMOTION-MISSING` (exit 2 of a
`reconcile` or `plan`: an environment's `fromPromotion` source, or a
`fromPromotion` binding, whose path has no record yet; the fix names `nyl
promote <path>`); `NYL-PROMOTION-SEQUENCE-OLDER` (exit 1: a `record: source`
block, or (M11) a target's record, with a lower `sequence` than the one
applied; the fix names `nyl promote <path> --revision`);
`NYL-PROMOTION-RECORD-INVALID` (exit 1: a `record: source` block that does
not match its path (the path's source environment, its values and their
selectors) or the source state it names, names a state the source's
history does not hold, names a path the Environment is not promoted along,
or reuses the applied sequence with another record);
`NYL-PROMOTE-MARKER-UNWRITTEN` (exit 1: the `record: source` commit was
pushed, but its marker in the target's state was not written; the promotion
stands, and promoting again writes the marker);
`NYL-PROMOTION-RECORD-UNSIGNED` (exit 1: with `fromPromotion.verify`, the
commit that last changed the block carries no signature an allowed signer
made, or its provenance cannot be read);
`NYL-PROMOTE-SOURCE-UNWRITABLE` (exit 1: `nyl promote` cannot commit or push
a `record: source` block, such as from a detached HEAD, a branch ahead of its
upstream, or a push the remote refuses for branch protection; each cause
has its own fix); `NYL-PROMOTE-SOURCE-REJECTED` (exit 2: the remote refused
the fast-forward push because the branch moved, and the commit is undone;
the fix is to update the checkout and promote again);
`NYL-PROMOTE-SOURCE-UNCERTAIN` (exit 2: the push of a `record: source`
commit failed without a refusal, such as a network error after the upload,
so it may have reached the remote; the commit stays on the local branch and
no marker is written into the target's state, because the record is not
known to be published. Nothing is known to have failed, and promoting again
is safe: the fix is to fetch the branch, check whether the commit reached
the remote, and run `nyl promote <path>` again, which finds the pushed block
`unchanged` and writes only the marker, or promotes again);
`NYL-PROMOTE-EVIDENCE-OVERRIDE` (a warning: `--evidence published` below the
path's level, recorded as an override); and `NYL-PROMOTE-UNCHECKED` (a
warning: a value without a known source commit, or a record whose commit
cannot be fetched, which the supersede check cannot check);
`NYL-PROMOTE-SHALLOW-CLONE` (exit 1: the supersede check needs the full
history, so a shallow entry clone is refused; the fix is `git fetch
--unshallow`, or `fetch-depth: 0` with `actions/checkout`).

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
  changeGate: none             # pullRequest from M11
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
  and a name its scope does not declare. Drift detection writes no receipt
  and attests nothing.
- A failing attestation for a state blocks its promotion on every path,
  whether or not the path requires it. For a path that promotes only values,
  a publication unit's `accepted` and `healthy` are judged per covered
  deployment from their details (see [Coverage](#kubernetes-publications)):
  an aggregate that fails only because deployments run different
  publications of the source does not block it. A later passing attestation of the same
  name clears it; `nyl promote --ignore-attestation <unit>/<name>=<reason>`
  (or `environment/<name>`) overrides it, and the PromotionRecord keeps the
  reason.
- One rule selects values: promote, per value, the newest source state whose
  evidence proves the required level. Every attestation names the state it
  attests; a KubernetesPublication's `accepted` and `healthy` also name, per
  deployment (each generated Application), the publication it runs, so at `attested` each value comes from
  what its consumer ran when it was attested.
- `nyl promote` never observes anything itself: it reads only attestations
  recorded beforehand, so it runs from a workstation without cluster
  credentials. Drivers record them during `reconcile` or when `nyl attest -e
  <env>` asks them, typically in CI where Argo CD access exists, and
  `nyl attest --name … --result …` records them from people and external
  systems. A path may set
  `maxAttestationAge`, such as `1h`, beyond which a recorded attestation no
  longer counts. The attestations used are stored in the PromotionRecord.
- Which states the values come from depends on the path:
  - A path that promotes the target's source takes the source commit and every
    value from one source state (see
    [Promoting an environment's source](#promoting-an-environments-source)). At
    `published`, that is the newest state at which every selected unit has a
    matching receipt or, for a target source, the newest publication commit;
    at `attested`, the newest state whose current receipt's running-scoped
    attestations pass, together with every other required attestation
    recorded for that state (see Choosing the state below). Mixed
    revisions block.
  - A path that promotes only values takes each value from the state its own
    consuming deployment or unit runs, which may differ between values when
    manual syncs left deployments at different revisions. Each value is then
    one its consumer really ran; the PromotionRecord lists each value's state.
    A value selected as a publication unit's input of a Release,
    `{unit: <publication>, input: /releases/<group>~1<release>/…}`, is
    consumed by that Release's deployment (its Application), as the unit's
    driver maps it (`Driver::input_consumer`); any other selector is consumed
    by the producing unit itself, so its value comes from that unit's newest
    receipt that meets the level. Following the consumer is the value-only
    selection rule, not an evidence requirement: it holds at `published`
    and under `--evidence published` too, whenever the publication's
    current receipt has a recorded `accepted` or `healthy` that lists the
    consumer. A deployment's publication matches a state whose receipt
    published either the commit the driver matched or the revision the
    deployment runs; when the driver's matching stopped at its walk limit
    (`publicationTruncated`), the matched commit may be newer than the one
    that introduced what runs. When no recorded state published it, the
    promotion is refused with `NYL-PROMOTE-NO-EVIDENCE`, "no recorded state
    published what <deployment> runs (<commit>)", and the fix names
    `--state-revision` or publishing again.
  - `--state-revision <desired commit>` selects an exact state and
    `--revision <source commit>` the newest with that source commit, for all
    values of the promotion. An explicitly chosen state reaches `attested`
    through the attestations recorded for it (see
    [Promoting an environment's source](#promoting-an-environments-source));
    only when those do not reach the path's level, such as attestations
    older than `maxAttestationAge`, does it need `--evidence published`,
    recorded as an override, for example to roll back. Given without an
    explicit state, `--evidence published` takes the newest state that reaches
    `published`, and the record carries an override only when that state is
    below the path's level. It never clears a failing attestation, and a
    failing attestation stays failing however old it is: only a later passing
    attestation of the same name or `--ignore-attestation` clears it, and
    `--ignore-attestation` sets aside failing attestations only. A commit the clone
    lacks is fetched from the source repository like a recorded source
    commit.
- `nyl promote dev-to-staging [--value …]` writes a
  PromotionRecord into the target environment's desired state: per value, its
  selector, the source unit's identity and incarnation, the source revision it
  came from, and its receipt or input digest; plus the attestations that proved
  the set at `attested`. With `changeGate: pullRequest` (M11) it opens a
  reviewable change instead.
- Promoting a path moves all of its values together by default. Selecting a
  subset (`--value`) is explicit and leaves the other values at their previously
  recorded lineage: each carried value keeps the state it came from (the old
  record's `from`, unless the value already names its own `state`). A path that
  promotes the target's source moves its source commit and every value
  together, so `--value` is refused there with `NYL-PROMOTE-SELECTOR-INVALID`,
  and so is a name the path does not declare.
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
      # verify: {allowedSigners: .nyl/allowed_signers}   # with `record: source`, see below
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
    attestations all passed. Environment-wide attestations, external ones
    (`nyl attest --name`), and those of other units name a state or a
    receipt, so an older state whose attestations passed qualifies. A
    publication unit's attestations are running-scoped: they are produced
    by reading what its reconciler runs, so they say what runs now, and
    only the attestation of the unit's current receipt counts. An older
    state does not qualify through its publication attestations once a
    newer receipt of that unit exists, and a newest attestation that fails,
    such as one that shows deployments on different publications, refuses
    the promotion with the driver's reason, without falling back to an
    older state whose attestations once passed, because they no longer
    describe what runs. The current receipt is the one the newest recorded
    state observes, even when that run did not finish the unit; when the
    newest state has no receipt of the unit (removed or torn down), no
    older state qualifies through its publication attestations. Which attestation names are running-scoped is the
    driver's declaration (`Driver::running_attestations`; a
    KubernetesPublication that declares `observe` names `accepted` and
    `healthy`), so the engine reads no driver details. Without a reader (a
    publication without `observe`), nothing changes.

    The cost is accepted: while a newer receipt is published but its
    deployments do not run it yet, an older state they still run cannot
    be promoted without flags, even if it passed. `--state-revision`
    names it explicitly, and it is then judged on the attestations recorded
    for it, as below; the refusal names that state and the flag.

  - When the chosen state and its values are what the path's record already
    names, and that record supplies every value of the target, `nyl promote`
    writes nothing and exits 0 (`outcome: unchanged`). Otherwise, such as
    after another path recorded since, it is a promotion like any other.
  - `--revision <source commit>` takes the newest qualifying state with that
    source commit, and `--state-revision <desired commit>` takes exactly one.
    An explicitly chosen state satisfies `attested` through attestations
    recorded for that state, such as those a publication unit that declares `observe`
    wrote while it ran, whether or not a newer receipt exists since; the
    record names them, with their times. Only a state without them needs
    `--evidence published`, recorded as an override.
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
  with their commits and source commit, which units have a current receipt
  and the names of the artifacts those receipts list, and the recorded
  attestations;
  `nyl get promotion-candidates <path>` evaluates the path's source states
  against the path with the rules and the reading of state `nyl promote`
  uses (a value-only path per value, as `nyl promote` selects a state per
  value), showing the level each reaches (up to the path's) or why not, the
  values it would carry, the state `nyl promote` would take now or the code
  it would refuse with, which one the path's record names, and which one
  the target runs now. It does not verify artifacts, so it cannot report
  `NYL-PROMOTE-ARTIFACT-MISSING`. `nyl promote
  … --dry-run` shows the chosen state and the exact change without writing
  anything. `nyl get promotions -e <env>` lists the environment's records and
  every promotion in its desired history (a `record: source` promotion
  through its marker), and `status` shows the lineage of
  what a promoted environment runs: the applied record's path, sequence, and
  source commit, the source state it promoted while that record is still
  the path's current one, and the sequence of a newer record no run applied
  yet (`status` reads only the current snapshot, so a replaced record's
  source state is shown by `nyl get promotions` instead).
  Each reads a history after one sync, so its round trips to the state
  remote do not grow with the history.
- Evidence covers the whole source, not only the selected values: in that
  state, every unit the target environment also selects has a current
  receipt (a target whose source is promoted along the path selects units
  as its own definitions at that state's source commit say, the commit it
  would run, not as the entry worktree has them; a commit at which they do
  not render keeps the state out), and at `attested` every such unit's required attestations pass for
  that state. A publication unit's `accepted` and `healthy` aggregate all
  deployments it generates (for a KubernetesPublication, its Applications):
  `accepted` when all of them run that
  publication, `healthy` when all are also Healthy. Mixed revisions block,
  because one source commit must fit every value. A path's
  `coverage.<unit>.exclude` names deployments of that unit it knowingly
  accepts as falling short; they leave the unit's aggregate for that path and
  are listed in the PromotionRecord's `excludedDeployments` under the unit. A publication unit without
  `observe` declares no attestations and is satisfied by its receipt.
- `record` decides where the PromotionRecord lives. With `record: state`, the
  default, it is written into the target's desired state, as a pull request
  against the desired ref under `changeGate: pullRequest` (M11). With
  `record: source`, `nyl promote` writes it into the Environment's own
  `source` block, as a pull request against source under
  `changeGate: pullRequest` (M11):

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
        values:
          webImage:
            value: registry.example.com/web@sha256:4f0c…
            select: {unit: web-image, artifact: image, pointer: /reference}
            unit: {name: web-image, uid: 2b90…}
            executionKey: sha256:…
        evidence:
          level: attested
          attestations:
            - {scope: kubernetes, name: accepted, result: pass, at: 2026-09-25T16:40:00Z, details: {…}}
            - {scope: kubernetes, name: healthy, result: pass, at: 2026-09-25T16:40:00Z, details: {…}}
          override: null
        ignoredAttestations: []
        excludedDeployments: {}
        artifactsVerified: true
        superseded: []                 # commits replaced with --supersede, with reasons
        requested: {by: …, identity: …, source: …, at: 2026-09-25T16:41:00Z, reason: …}
  ```

  The source commit and its values are then one reviewed change. Without a
  change gate, `nyl promote` commits the block to the branch the entry
  worktree has checked out and pushes it fast-forward only; a detached HEAD
  is refused with the fix to check out a branch, and a concurrent change is
  a refused push that the job retries. The block is part of `source`, so it is read from the
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
  - **Marker.** After the source commit is pushed, `nyl promote` writes one
    commit into the target's desired state under the lease it holds: the
    same `promote` summary and trailers a `record: state` promotion writes,
    and no `desired/promotions/<path>.yaml` file. It changes no document,
    so it is no state commit, and `status` and the lineage read nothing
    from it; the target's history then reserves the sequence, so a
    checkout on another branch takes the next one, and `nyl get
    promotions` lists the promotion in its history. The order is fixed:
    source first, marker second. A marker that cannot be written leaves the
    promotion standing (the pushed block is the record): `nyl promote`
    reports `done` with the source commit and exits 1 with
    `NYL-PROMOTE-MARKER-UNWRITTEN`. Running it again from a checkout of the
    pushed commit finds the block `unchanged` while it still names the
    state it would promote, and writes only the missing marker, under the
    re-runner's `Nyl-Requested-By` and `Nyl-Reason`; if a newer state
    qualifies by then, it promotes that instead. A marker is written for an
    existing block only when the checked-out branch is not ahead of its
    upstream and the Environment's file is unchanged, so the block is the
    one the upstream carries; otherwise `NYL-PROMOTE-SOURCE-UNWRITABLE`.
    A push that fails without a refusal may have reached the remote, so
    no marker is written until a re-run finds the block published
    (`NYL-PROMOTE-SOURCE-UNCERTAIN`, exit 2): the marker reserves a
    sequence only for a record known to exist, and that re-run's repair
    writes it. `--dry-run` writes neither.
  - **Trusting the block.** By default a run that first applies a new
    block reads the source environment's state and checks the block
    against the state it names, so it needs read access to that state.
    `verify: {allowedSigners: <path>}` in `source.fromPromotion` (a Git
    `allowed_signers` file, a local path of the project) trusts the block
    by its signature instead: the commit that last changed the block, as
    `git log -L` follows the block's lines back from the entry commit, must
    carry an SSH signature `git verify-commit` accepts with
    `gpg.ssh.allowedSignersFile` set to that file (through a merge, the
    side commit that changed the lines counts, not the merge). Only the
    commit's `gpgsig`/`gpgsig-sha256` headers are read, every one must be
    an SSH signature, and `verify-commit` runs with OpenPGP and X.509
    programs that cannot start, so no keyring is consulted. The block's
    lines, dedented, must parse to the declared block. The clone must not
    be shallow (its boundary commit would be credited with the whole
    block), replace objects are ignored, and the file must hold what the
    checked-out commit carries. The block is still checked against its
    path, and the source's state is not read. Any failure refuses with
    `NYL-PROMOTION-RECORD-UNSIGNED` (exit 1). `nyl promote` verifies its
    own commit the same way before pushing and refuses to push one a run
    would not trust, and its dry run checks that SSH commit signing is
    configured. It needs Git 2.34 and OpenSSH 8.2. `valid-after` and
    `valid-before` are checked against the committer date the signer
    chose; `gpg.ssh.revocationFile` withdraws keys. Nothing enables it
    implicitly. The `verify` setting and the allowed signers file are read
    from the same commit the run checks, so anyone who can push to that
    branch can change whom it trusts: `verify` is only as strong as the
    branch protection on the Environment's file and the signers file, and
    the documentation says so. Reading both as of the commit the target
    last applied would remove that dependency; it is not implemented.
- Either way, the source commit and the values sit in one record, so they
  cannot diverge. An environment that follows its entry worktree, or a
  revision, is not a promotion target for its source.
- Before the first promotion, an environment with a `fromPromotion` source has
  no source commit, and `reconcile` exits 2 naming `nyl promote`.
- The target may rebuild a unit dev also runs, such as `web-image`, at the
  promoted commit, or bind dev's result with a `fromPromotion` binding.
  Either is an explicit choice of the target's configuration, and Nyl does
  not warn about either, nor about a target that does both. The
  PromotionRecord states exactly the values it carries and the evidence for
  them; it says nothing about what the target builds itself, whose image
  tags include the environment.
- A target source (`from: {target: …}`) promotes the source commit its
  publication recorded, under the same rules.
- Prod-only changes, such as a replica count, reach prod with the next
  promotion of a commit that contains them. Hotfixes that cannot wait for dev
  take a path of their own (see [Hotfixes](#hotfixes)).
- A path that promotes a source may narrow the deployments that count toward
  a unit's running-scoped attestations to named groups, such as
  `coverage: {kubernetes: {groups: [web]}}` for a publication's
  ApplicationGroups. It never narrows what moves; the promoted commit still
  carries every group's definitions.

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
  value that another path wrote since P last promoted it. A commit V contains
  a commit R when every commit reachable from R and not from V, merge
  commits aside, has a commit with the same changes (`git patch-id`) in V's
  history since their merge base: `git cherry V R` lists none with `+`. An
  ancestor is contained, and so are clean cherry-picks and single-commit
  squash merges; a squash of several commits is equivalent to none of them.
  The commits it lists are the missing ones the refusal names.
- **Source commit of a value.** A promoted source commit, such as an
  environment's source or a unit's `webSource`, is its own. An artifact or
  output has the source commits that its producing receipt's desired unit
  resolved, such as an OciImage's Git contexts, so an image built before a
  hotfix cannot replace the hotfixed image even on a path that promotes only
  digests. Commits are compared within their own repository. A value without a
  known source commit, such as an image from outside Nyl or a command output,
  is listed as unchecked (`NYL-PROMOTE-UNCHECKED`) by `nyl promote`, and by
  `plan` of the target for every value that two or more paths into it
  promote, paired by value name (the names path-less bindings read). That
  listing is static, judged from the source environment's rendered units
  rather than a recorded state: an artifact or output of a unit with no
  Git source field, or an input that neither lies in one nor is filled by
  a `fromUnit` reference to such a unit. It can therefore differ from what
  `nyl promote` finds in a state, such as a reference to another
  environment's unit, which it lists as unchecked. A
  subset promotion checks only the values it writes. Commits are paired by
  the source field they came from (an image's main context with the other
  image's main context), and with any commit of the repository only when no
  field matches.
- **Refusal.** A refused promotion writes nothing and exits 2 with
  `NYL-PROMOTE-SUPERSEDES`, because nothing failed: the promote job succeeds
  once the missing commits are merged. The message lists the missing commits
  per repository in topological order and the entries to copy, with full
  commit IDs: "`S5` is missing 2 commits from `hotfix-to-prod`: a1b2 fix
  pool size, c3d4 bump timeout; merge them or pass `--supersede
  a1b2…^..c3d4…=<reason>`". When the oldest missing commit is a root, which
  has no parent, each missing commit is an entry of its own. A shallow entry
  clone cannot answer the check and is refused with
  `NYL-PROMOTE-SHALLOW-CLONE` (exit 1), whose fix is `git fetch
  --unshallow` or `fetch-depth: 0`.
- **Override.** `--supersede <commit or range>[=<reason>]` accepts that the
  promotion replaces those commits, because the change was made differently
  or is dropped on purpose. It is repeatable; an entry without its own reason
  takes `--reason`, and an entry with neither is rejected. Ranges are Git
  ranges resolved at promotion; commits in them that are not missing are
  ignored, and a missing commit no entry covers keeps the promotion refused.
  An entry may be qualified with its repository, `--supersede
  web:S1..c3d4=<reason>`, by its name (below) or a URL that normalizes to
  its URL; the refusal prints the qualifier for every repository but the
  project's own when only that one is involved. An unqualified entry belongs
  to the one repository whose missing commits it names (all repositories
  share the entry clone's objects, so a commit ID resolves in each); one
  that names missing commits in several must be qualified.
- **Unfetchable records.** A record's commit that cannot be fetched, such as
  from a deleted branch, is unchecked (`NYL-PROMOTE-UNCHECKED`, naming the
  record), not a refusal; `--supersede <full commit ID>=<reason>` naming
  it records that the promotion replaces it. Commits are fetched into the
  entry clone without reading their files.
- **Record.** The PromotionRecord lists every superseded commit with its
  repository URL, its reason, and who gave it, never the range, and a pull
  request under `changeGate: pullRequest` (M11) shows them. A later promotion on P
  is no longer checked against them, because P has now promoted after them.
  A repository is named, in the record, in refusals, and as a qualifier, by
  the GitRepository whose URL matches its URL, else by its normalized URL;
  the project's own repository by its `origin` URL, or `project` without
  one. A URL is normalized without credentials, scheme, trailing `.git` and
  `/`, with `git@host:org/r` as `host/org/r` and the host lowercased, so
  `https://Git.example/org/r.git` and `git@git.example:org/r` are both
  `git.example/org/r`.
- **Which records.** The records are the target's current ones and those its
  desired history holds at each `promote` commit, so one `state restore`
  removed still counts. With `record: source`, earlier blocks are commits in
  source, and only the current block counts, so the two modes can differ
  after another path rolled back: with `record: state` a promotion is still
  checked against the rolled-back record, with `record: source` it is not.
  "Since P last promoted" counts P's records from history too. A record
  whose commit is an ancestor of a newer record of the same path is not
  checked on its own. A record's value has the source commits its producing
  unit resolved in the state the record names, read from the source
  environment's state history; a state that history no longer holds, or an
  environment no longer declared, leaves the value unchecked, and any other
  failure to read state is an error.

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
  unitSelector: {matchLabels: {hotfix: 'true'}}   # web-image and kubernetes, on a target of its own
  values: {target: hotfix}
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
  It builds and runs the fix on a small target of its own, on a Cluster or
  Argo CD instance of its own, because it cannot
  publish into prod's target, and can stay idle between hotfixes. Its own
  label selects the units it runs: the image prod reuses (`web-image`, which
  prod does not select) and the publication, so the path's selector names a
  unit the source runs. A binding prod's target reads from a unit
  `prod-hotfix` does not run, such as a database output, takes a static
  value on the hotfix target.
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

This section is M11; M6 rejects a target in `from`.

`from` selects either an environment, as above, or a
DeploymentTarget. A target source lets a dev target that renders from `value`,
`fromGit`, or `fromPublication` bindings (including `carryFileFromWorktree`) feed an
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
  attestation details that proved the set.
- `to` may be an environment or a target; a target keeps its records in its
  own source (see [Promoting into a target](#promoting-into-a-target)).

### Promoting into a target

This section is M11; M6 rejects a target in `to`.

A DeploymentTarget that belongs to no environment is a promotion target of its
own. It has no desired state, so its records live in its source, as
`record: source` does for an environment:

```yaml
apiVersion: gitops.nyl/v1
kind: PromotionPath
metadata: {name: dev-to-production}
spec:
  from: {target: dev}
  to: {target: production}
  evidence: attested
  changeGate: pullRequest
  values:
    webImage:
      select: {input: /releases/platform~1web/image}
---
# DeploymentTarget production
spec:
  releaseInputs:
    platform/web:
      image: {fromPromotion: {path: dev-to-production, value: webImage}}
  promotions:                        # written by nyl promote, never by hand
    dev-to-production:
      path: dev-to-production        # the one PromotionRecord shape, as in state
      sequence: 4                    # increases with every promotion into this target
      from: {target: dev, repository: https://git.example.com/deploy.git, branch: deploy/dev, commit: 9c1e…}
      values:
        webImage:
          value: registry.example.com/web@sha256:4f0c…
          select: {input: /releases/platform~1web/image}
          inputDigest: sha256:…      # `unit: {name, uid}` appears when the value has a source unit
      evidence:
        level: attested
        attestations: [{scope: dev, name: accepted, result: pass, at: 2026-09-25T16:40:00Z, details: {…}}, {scope: dev, name: healthy, result: pass, at: 2026-09-25T16:40:00Z, details: {…}}]
        override: null
      ignoredAttestations: []
      excludedDeployments: {}
      superseded: []
      artifactsVerified: true
      requested: {by: …, identity: verified, source: …, at: 2026-09-25T16:40:00Z, reason: …}
```

- **Record.** `spec.promotions.<path>` holds the path's PromotionRecord with
  the fields of [PromotionRecord](#promotionrecord). `nyl promote` writes it
  into the DeploymentTarget file, addressed by document position as
  `nyl update source-locks` edits locks, and commits it to source; under
  `changeGate: pullRequest` it opens that change as a pull request instead.
  The source commit and the values it renders are then one reviewed change.
  The record never lives on the publication branch, where rendering would read
  its own output without review.
- **Values only.** A target renders the source it is published from, so a path
  into a target promotes values, never a source commit.
- **Bindings.** `fromPromotion` bindings of the target read `spec.promotions`,
  with the rules of [Several paths into one environment](#several-paths-into-one-environment):
  without `path`, the record with the highest `sequence` that carries the
  value. `render-tree`, `publish-tree`, `diff-tree`, and direct commands
  resolve them without orchestration, and a binding whose path has no record
  yet blocks with a message naming `nyl promote`.
- **Rollback protection.** The ownership index records each applied record's
  `sequence`. `publish-tree` refuses a record with a lower sequence than the
  published tree applied, such as one from an old checkout or a reverted
  promotion commit, and names `nyl promote --state-revision` as the way to roll
  back.
- **Selection and checks.** Evidence, state selection, the supersede check
  across paths into the target, and artifact verification follow the rules
  above. `nyl promote` takes no lease: its source commit must fast-forward the
  branch holding the target file, so a concurrent promotion is refused and
  retried.
- **Scope.** A target that a KubernetesPublication unit places into an
  environment is not a `to` target; it receives promotions through that
  environment's desired state.

A target can also follow another target's published state with a locked
`fromGit` binding. `nyl update source-locks` moves such a lock to the branch
head, with no health gate and no record: it follows the branch, not what the
source target runs.

## Health evidence

### Attestations

Evidence beyond a receipt is an attestation: a named pass or
fail result about one unit's current receipt, or about an environment's
recorded state. It is recorded under its name in `observed/units/<unit>/attestations.yaml`
or `observed/attestations.yaml` with its result, time, details, and
source. The unit's driver, not Nyl's core, knows how its health is observed,
so every source writes the same record:

| Source | When | Examples |
| --- | --- | --- |
| The driver, during `reconcile` | The apply itself proves it | A Terraform apply whose `check` blocks or health waits pass; a command unit that reports its result |
| The driver, when `nyl attest -e <env>` asks | Observed afterwards | A KubernetesPublication's `accepted` and `healthy` from Argo CD; configured probes for a unit such as an OpenTofu-managed ECS service; a command unit's `attest` step |
| External, through `nyl attest --name … --result …` | Someone or something else knows | A monitoring hook, a manual QA pass, an end-to-end job |

- **Declared names.** A driver reports, from a unit's spec, which attestations
  it can produce: a KubernetesPublication that declares `observe: {}` declares
  `accepted` and `healthy`; an OpenTofu unit declares `healthy` once its spec configures
  apply-time checks or probes, and nothing otherwise. A unit's
  `spec.attestations` adds names only external sources supply, such as
  `[{name: healthy, from: external}]` for a service nothing in Nyl can
  observe, and `Environment.spec.attestations: [qa]` declares environment-wide
  ones. No name is reserved; `healthy` is a convention of the drivers.
- **External attestations.** `nyl attest -e <env> [--unit <u>] --name <n> --result
  pass|fail [--reason …] [--url …]` records an attestation for the unit's
  current receipt, or without `--unit` for the environment's current state;
  attesting an earlier state with `--state-revision` or `--revision` is M11,
  because the files hold the latest attestation of the current receipt or
  state, and an earlier state's entry would replace it. It refuses names
  the scope does not declare, so a typo cannot create evidence nobody
  requires. The attester is recorded like an approver, through the same
  identity and approval sources. `nyl attest --target <name>` writes to a
  non-orchestrated target's `_nyl/attestations.yaml`. Without `--name`,
  `nyl attest` asks the drivers instead (see the orchestration core
  contract's [Attestations](orchestration-core.md#attestations)); that form
  is implemented, and so is the external form (M6); `--target` (M11) is not yet.
  Environment-wide attestations live in `observed/attestations.yaml`, a file
  the observed layout of format `v1` always listed, added without a version
  change; a Nyl older than the release that writes it keeps it but cannot
  act on it, as for `desired/promotions/` above. An environment-wide
  attestation's `executionKey` is the state's desired commit: the newest
  commit of the desired ref that changed the environment's `desired/`
  documents, not the ref's tip (see the orchestration core contract's
  [Attestations](orchestration-core.md#attestations)).
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

A KubernetesPublication that declares `observe: {}` attests `accepted` and
`healthy` by observing the reconciler that runs its target: Argo CD, or Nyl's
own direct reconciler (see [Reading a direct target](#reading-a-direct-target)).
Nyl knows the Applications it generates for a target, their ArgoCDInstance, and
its Cluster; for a direct target it knows the Releases it applies and the
Cluster they go to. Wherever this contract says Application, a direct target's
deployment is a Release, identified as `<group>/<release>`, and coverage names
deployments. `observe` is an object so that options can be added later; only
`{}` is accepted. It opts the unit into observing, and says nothing about which
reconciler runs: that follows the target's reconciler, so the declaration does
not change when the reconciler does. A direct target is read afterwards by
`nyl attest`, like an Argo CD target; `reconcile` applies it and attests
nothing itself, so there is one evidence path.

- **Observer.** Nyl reads the target's generated Applications from Argo CD,
  one of two ways that an `ArgoCDInstance` declares in `spec.access` (see
  [Reading Argo CD](#reading-argo-cd)). Health checks need credentials for
  Argo CD, which publication does not, so attestations are recorded where
  those credentials exist, typically in CI by `nyl attest`, and decisions read
  the recorded evidence.
- **Where attestations are recorded.** An orchestrated environment's
  publication units record into its observed state. A target without an
  environment records into its own publication branch:
  `nyl attest --target <name>` writes the target's `accepted` and `healthy`
  in `<prefix>/_nyl/attestations.yaml`, whose details list each
  Application's running revision, matched publication, and health.
  - It commits only when that content changes or the previous attestation is
    older than a refresh interval, which keeps `maxAttestationAge` satisfiable
    without a commit per run.
  - `_nyl/attestations.yaml` is a reserved path: never rendered, never removed by a
    publish, and neither owned nor unowned for reconciliation; a teardown of
    the target removes it. No Argo CD Application sources it, and attest and
    publish commits touch disjoint files, so the branch's retry rule covers
    concurrent writes.
  - The branch history keeps older attestations, so an older publication can
    be promoted or locked from recorded evidence.
  - This adds no refs: attestations live with the tree they describe.
- **Running revision.** Argo CD's Synced/OutOfSync status compares against the
  branch head, so an Application still running an older commit reports
  OutOfSync as soon as a newer publication changes its files. Nyl ignores it.
  The running revision is that of the newest `status.history` entry (the
  highest `id`), which Argo CD appends only for full, non-dry-run syncs. The
  sync result is not used, because Argo CD also records selective and dry-run
  syncs there as succeeded. An operation as new as that entry (times have
  second resolution) that is not a completed full sync leaves the Application
  running a mix of revisions, which proves no revision: a selective sync
  (`operation.sync.resources`), a full sync that failed, errored, or is still
  running (it may have applied part of its revision), an operation that is no
  sync, one whose time cannot be compared with the entry's, and a dry run.
  A dry run changes nothing itself, but `status.operationState` holds only the
  latest operation, so a dry run after the entry may hide a selective sync
  before it. Health is Argo CD's live health, `status.health.status`; only
  `Healthy` is healthy.
- **Matching a publication.** The running revision R may be another target's
  commit or a state write-back on a shared branch. For each covered
  Application, Nyl finds the source target's publication commits (commits that
  change `<prefix>/_nyl/index.json`) that are R or an ancestor of R along first
  parents. The newest of them must have the Application directory tree and
  recorded inputs for that Release that R has (so a commit that restores an
  older publication's tree by hand is `unmatched`, not credited to that
  publication); the commits equivalent to it form the run below. The directory tree is Git's tree for the
  Application's `source.path`, so it covers every file Argo CD reads there;
  the recorded inputs are the ownership index's `compilation` record for the
  Release. Values are always recovered from a matching publication commit,
  never from R. A revision that no publication commit matches, such as one
  that edited the Application's directory without publishing, or one not on
  the publication branch, is `unmatched`, never credited to the publication
  before it.
- **Accepted and healthy for a receipt.** A unit's attestations name its
  current receipt, whose `PublishedTree` records the publication commit C.
  An Application is *accepted* when its running revision's directory tree and
  recorded inputs equal those at C: it runs this receipt's publication,
  whatever commit it reports. `accepted` passes when every Application the
  target generates, the catalog Application included, is accepted, and
  `healthy` when, besides, every one is Healthy. Any other state fails the
  aggregate and is listed in the details with its reason: `other-publication`
  (it runs another publication), `unmatched`, `not-on-branch`, `mixed`,
  `never-synced`, `missing`, `deleting`, `other-source` (its `spec.source`, or
  the source of its last sync, differs from the generated one without
  `targetRevision`: another directory, or `directory.recurse`, `exclude`, or a
  tool block edited in the cluster), or `unreadable`. A target that generates
  no Application attests both vacuously. The details record the
  reconciler that was read (`reconciler`: its kind, instance, namespace, and
  access) and, per Application (`deployments`), its id (its name, qualified by
  its namespace outside the instance's), role, directory, state, health
  (`Healthy`, `Progressing`, `Degraded`, or `Unknown`, with the reconciler's
  own word in `reported` when it differs, shown and never judged), the revision it runs,
  and the publication it matched (the recorded commit below), and carry no time
  of the reading, so an unchanged system attests the same document and writes
  no commit.
- **Recorded commit.** The running publication is what gets promoted. Among the
  publication commits equivalent to it, the PromotionRecord (or the lock's
  `commit`) names the oldest in the unbroken run of matching commits ending at
  R: the commit that introduced what runs now. A change that was later reverted
  starts a new run, so a tree that went A → B → A records the second A. When no Application changed between the publications that
  different Applications run, every value names the same commit, so the audit
  trail stays a single commit; otherwise each value names the commit that last
  changed its own Application.
- **Coverage.** One publication unit has one receipt and one publication
  commit, and runs as several deployments that each run independently, so a
  PromotionPath's `coverage` is per unit: keyed by the unit's name, with
  `groups`, `exclude`, and `require` naming the unit's deployments and their
  groups as its driver lists them (`Driver::deployments`). For a
  KubernetesPublication a deployment is a generated Argo CD Application (a
  Release, `<group>/<release>`, on a direct target), named by the id the
  attestation details give it, and a group is the ApplicationGroup of its
  Release; the catalog Application belongs to none. For example:

  ```yaml
  spec:
    coverage:
      kubernetes:                 # the publication unit
        groups: [web]
        exclude: [web-batch]
        require: [web-gateway]
  ```

  For a path that promotes only values, the deployments whose Releases
  produced the promoted values must meet the required `accepted` or
  `healthy`, and a unit's `require` adds deployments that must meet them at
  whatever publication they run, without contributing values. For a path
  that promotes the target's source, coverage is every deployment of every
  publication unit the target also selects, aggregated per unit as described
  above; a unit's `groups` narrows it to the deployments of those groups, and
  its `exclude` leaves the named deployments out, so a failing aggregate that
  only they explain does not block. An entry counts only for the unit it is
  keyed by. `groups` applies only to source paths: a value-only path judges
  each value for its own consumer, so validation refuses `groups` there with
  `NYL-PROMOTE-COVERAGE-INEFFECTIVE`, as it refuses any entry of a unit
  whose kind has no running-scoped attestations for its spec (such as a
  publication without `observe`). `nyl validate`, `nyl plan`, `nyl reconcile`, `nyl promote`, and `nyl get promotion-candidates` check every name against what the
  unit's driver lists for the worktree, through the same path checks, and
  refuse a unit the source does not declare, select, or enable, a unit
  whose kind lists no deployments, and a group or deployment the unit does
  not run with `NYL-PROMOTE-COVERAGE-UNKNOWN`. When the driver cannot list
  them, such as when the target does not render, the names are unchecked
  (`NYL-PROMOTE-COVERAGE-UNCHECKED`): an error in `nyl validate`, a warning
  in the others, which judge another environment only by what they need. A
  KubernetesPublication lists the deployments its resolved spec recorded,
  or renders its target for validation, with each binding orchestration
  resolves taking a placeholder and `fromPublication` inputs read fresh or
  from the cached branch, so it works offline.
  Coverage still never fails open at promotion time, for what changed since:
  a named group no deployment of the attestation belongs to, a coverage that
  leaves no deployment to judge while the attestation fails, and a `require`
  name that no running-scoped attestation of the unit's current receipt
  lists (also without `observe`, and whatever the path requires) refuse the
  promotion with `NYL-PROMOTE-NO-EVIDENCE`, naming the field. On a
  value-only path, an excluded consumer must still run a publication of the
  source, which decides the value's state, but need not meet the attestation
  there. The per-deployment judgement reads a typed verdict the driver
  derives from its own details (`Driver::deployment_verdicts`: what each
  deployment runs, whether it meets the attestation there, and the driver's
  reason), never the details themselves, and nothing of it is persisted.
- **Where Application data lives.** Only a `KubernetesPublication` unit knows
  Applications, because it generated them; it records them at resolution
  (see [Reading Argo CD](#reading-argo-cd)). Its `accepted` and `healthy`
  attestations aggregate every Application's running revision and health for
  the matched publication and list them in their details; promotion
  decisions use the results, and the per-Application details explain why a
  unit fell short.
- **Manual syncs.** For a path that promotes only values, Applications synced
  to different publications do not block promotion: coverage is judged per
  covered Application from the current receipt's `accepted` and `healthy`
  (through the driver's typed verdicts), so a value is promotable when its
  consuming Application is accepted (and healthy, when required) for some
  recorded publication of the source, and the value comes from the state
  whose receipt produced that publication. Promotion blocks only
  when a covered Application runs a revision that matches no source
  publication (`unmatched`), or another per-Application finding fails it
  (`mixed`, `missing`, `other-source`), or it is not Healthy: a recorded
  `healthy` failing for the consumer blocks its value even when the path
  requires only `accepted`, as any failing attestation blocks its state,
  until `--ignore-attestation` sets it aside or the consumer is in its
  unit's `coverage.<unit>.exclude`. A path that promotes the target's source
  also blocks on mixed revisions.
- **Decision evidence is always recorded.** The attestations behind every
  promotion, with their details (time, Application, running revision,
  health), are stored in the PromotionRecord, including one kept in a
  target's `spec.promotions`; the attestations themselves are recorded in
  observed state or on the target's publication branch.
- **Attestation history.** One attestation proves what runs now. Promoting a
  commit that no longer runs, requiring a minimum healthy duration, and
  guarding against Applications that flap between healthy and unhealthy need
  attestations over time; Argo CD does not reliably report how long an
  Application has been healthy. Git history of the observed state keeps
  every attestation `nyl attest` recorded; periodic attestation is part of
  continuous operation.
- **Meaning.** Argo CD health means Kubernetes considers the resources ready,
  such as a completed Deployment rollout. It does not prove the application
  works. Application-level checks, such as HTTP probes, smoke tests, or manual
  QA, are further attestations: from a command unit, a driver's probes, or
  `nyl attest`.

### Reading Argo CD

The observer is one seam, a reconciler-neutral reader, with Argo CD's two
implementations behind the publication driver. A reader returns a neutral
reading of the deployments the target expects (for Argo CD, its generated
Applications): per deployment its running revision (one commit, a mix, or
nothing yet), health (`Healthy`, `Progressing`, `Degraded`, or `Unknown`), and
live source; per anchor whether it is visible; and, for an expected deployment
the reading lacks, whether the credential could have seen it. Everything after
the reading, the match to a publication, health, the attestations, and the
answer of a teardown wait, is shared and names no reconciler:

| Method | Declared as | Reads | Credentials |
| --- | --- | --- | --- |
| `kubernetes` (default) | `ArgoCDInstance.spec.access: {method: kubernetes}`, or no `access`, and the control-plane Cluster's `spec.live.context` | `applications.argoproj.io` objects in the Application namespaces, through the Kubernetes API of the instance's control-plane Cluster | The kubeconfig the unit admits as `KUBECONFIG`, at that context |
| `api` | `ArgoCDInstance.spec.access: {method: api, api: {server: https://argocd.example.com, tokenEnv: ARGOCD_AUTH_TOKEN}}` | `GET <server>/api/v1/applications?appNamespace=<ns>` | The token the unit admits under the name `tokenEnv` names (default `ARGOCD_AUTH_TOKEN`) |

- **Why both.** The Kubernetes method needs nothing beyond the control-plane
  Cluster's own access (the credentials a platform team already issues, and
  the only route when Argo CD's API is not exposed), but a kubeconfig that can
  list Applications is cluster access. The API method needs only a token
  scoped to `get` on the instance's Applications (an Argo CD account or
  project role), the least privilege a CI job needs, but needs the API server
  reachable from the runner. Which fits is the platform team's decision about
  one Argo CD installation, so it is declared once on the instance, not chosen
  per command or per runner: a CI render, an orchestration run, and a local
  `nyl attest` read the same way.
- **Declared in Git, on the instance.** The Applications are the Argo CD
  instance's, not the workload Cluster's, and one instance serves many targets,
  so the choice and the API server's address belong to the ArgoCDInstance:
  every target on it reads the same way. The Kubernetes method needs the
  control-plane Cluster's local context, which the Cluster already declares as
  `spec.live.context`; an instance without `access` and without explicit
  instances (the implicit one in the workload Cluster) therefore reads through
  that Cluster. `access` never affects rendered bytes: it is left out of every
  render input, and the ownership index hashes an instance that declares it
  without it, so editing `access` republishes nothing and resets no
  attestation (adding `access` to an instance, or removing it, changes that one
  digest once; the same holds for `live` on any other Cluster document, such as
  the control-plane Cluster's, which is hashed without it when it declares it).
  Resolution records the reconciler facts in the unit's resolved spec
  (`reconciler`): `kind: argocd`, the instance with its namespace and
  control-plane Cluster, its `access` and local `context`, the publication
  destination, the deployments the target expects (`expected`: per Application
  its id and display name, role, directory, Release, generated source, and, for Argo CD's
  reader alone, a locator with its namespace, name, and project), and the
  `anchors` described below. `attest` and the `observe` teardown wait read the
  stored document and render nothing. Only what the render derives keys the
  unit: the expected deployments, the anchors, the instance and Cluster names,
  and the destination's branch and prefix; the declared access, the
  control-plane context, and the URL the branch is read from are stored but
  excluded from the key, so renaming a kube context or moving the API server
  never re-executes the unit.
- **Adding a reconciler.** The reading and everything after it are neutral on
  purpose. The direct reconciler is the second kind (see
  [Reading a direct target](#reading-a-direct-target)); a third (Flux, say) adds a renderer deriving its
  expected deployments and anchors from what the target generates, a location
  variant recorded under `reconciler.kind`, and a reader that maps its API into
  the neutral reading; the matching, health, attestations, teardown wait, and
  the orchestration engine do not change.
- **Credentials only through the unit's admitted environment.** The
  declaration names the variable, never a value: `tokenEnv` for the token, and
  `KUBECONFIG` for the kubeconfig file or files. The unit admits it with
  `env.passthrough` (from the runner) or `env.secrets` (from the project's
  secrets provider), as any driver credential, and a credential that is not
  admitted is an error naming the variable and the `env` entry that admits it.
  Nothing falls back to the process environment, the kubeconfig's current
  context, `~/.kube/config`, in-cluster configuration, or a proxy from the
  process's `HTTPS_PROXY` (a unit admits `HTTPS_PROXY` for the API method; a
  kubeconfig's own `proxy-url` applies to the Kubernetes method). The API
  server's URL must use `https`, except for a loopback address, and the token
  is sent only to it and never followed through a redirect. One exception is
  stated rather than hidden: a kubeconfig's `exec` plugin or auth provider is
  started by the Kubernetes client with the whole process environment, as
  `kubectl` does, and the client offers no way to restrict it; what it reads
  is the kubeconfig's declaration, not Nyl's. Use the API method where
  that matters.
- **Observed values read the workload Cluster the same way.** A
  [`KubernetesPublication`'s observed values](orchestration-core.md#kubernetes-publication-unit)
  read a rendered resource from the target's workload Cluster, which may be
  another Cluster than the instance's control plane, through the workload
  Cluster's own `spec.live.context` in the admitted `KUBECONFIG`, once the
  Application that owns the resource runs the receipt's publication (matched
  as above). The same rules apply: only admitted credentials, no ambient
  kube configuration.
- **Deadlines.** Each reading ends within the check's deadline, at most 30
  seconds.

Failure modes, and what each does:

| Situation | Result |
| --- | --- |
| Argo CD unreachable, a server error, or a timeout | `nyl attest` reports the unit `unknown` (`NYL-ATTEST-UNKNOWN`, exit 2) and records nothing, so the evidence recorded earlier stands until `maxAttestationAge` retires it; the teardown wait is `observation-failed` |
| Token or kubeconfig credential rejected (HTTP 401), expired included | The same, with the reason naming the expiry and the secret or variable to refresh; a credential that cannot be told expired from invalid is reported as "invalid or has expired" |
| Credential accepted but may not read (HTTP 403) | The same, naming the permission to grant |
| A partial reading (one Application namespace of several fails, or the body is no complete Application list) | Unknown: a partial reading never stands for a complete one |
| An Application the credential cannot see | The reader says, for each expected deployment the reading lacks, whether the credential could have seen it; an absence the reader cannot vouch for fails the whole evaluation (never one deployment's finding), so unknown is never a pass and removal is never concluded from what could not be seen. Argo CD's list leaves out what a token may not `get` and still answers success, so absence from it proves nothing alone. For the API method, every generated Application missing from the reading is checked with `GET /api/v1/account/can-i/applications/get/<project>/<name>` (`<project>/<namespace>/<name>` outside the instance's namespace): a `no` or an error makes `attest` unknown and the teardown wait `observation-failed`, naming the permission to grant; only a `yes` lets the Application count as gone (`missing`, failing `accepted`) |
| A token or context that sees none of the target's Applications | The teardown wait concludes removal only if the reading shows what phase 1 keeps and only the Argo CD running this target has (the anchor), else `observation-failed` (the credential or context does not see this target: another project's role, a token without policy, another Argo CD's control plane). The reading's anchors are the things only the Argo CD running this target has: the catalog Application when the target generates one; for a target without one (`catalogApplication.shared` or `enabled: false`), the AppProjects its Applications belong to, which phase 1 keeps, must be visible (`appprojects.argoproj.io` through the Kubernetes API, `GET /api/v1/projects/<name>` through the API server, which needs `get` on those projects); `default`, which every Argo CD has, anchors nothing. A target with neither fails closed for the Kubernetes method (nothing shows its context reaches this target; declare method `api`), and relies on `can-i` alone for the API method; a target with no workload Application has nothing to conclude. The anchor comes before the per-Application `can-i` above, and once an anchor is not visible nothing more is asked of Argo CD, so the anchor's error and fix are what is reported; the shared evaluation, not the reader, decides that a target without a workload Application needs none. The same anchor makes `attest` with the Kubernetes method `unknown` instead of a recorded `missing` failure when its context points at another Argo CD (a target whose catalog was never applied is `unknown` until it is; the API method needs no anchor there, because `can-i` answers for each Application). A tampered `spec.project` on a live Application is not detected |
| An Application that does not exist | `missing`, and `accepted` fails; a teardown wait counts it as removed (once visibility is established as above) |
| The publication branch cannot be fetched, or the running revision cannot be compared | Unknown, like an unreachable Argo CD |
| The receipt's publication commit is no longer on the branch (rewritten) | `accepted` and `healthy` fail for every Application: nothing can run it |
| A stale `status.history` entry | History is read as Argo CD wrote it: the newest entry by `id` is the running revision, and any operation as new as it that is not a completed full sync (a selective sync, an unfinished or failed one, a dry run that may hide an earlier selective sync) makes the Application `mixed`; a `spec.source`, or an entry's `source`, that differs from the generated one makes it `other-source` |
| Several targets sharing a branch | The running revision may be another target's commit or a write-back; matching is by the Application's directory tree and recorded inputs, never by commit identity, so those commits match the publication whose directory they leave alone |

Only an observation that established a result is recorded: `fail` means Argo
CD was read and an Application fell short, `unknown` means it could not be read.
An unchanged result is rewritten with its new time once the stored attestation
is older than the refresh interval (15 minutes; see the orchestration core
contract's [Attestations](orchestration-core.md#attestations)), so a system
that is re-verified keeps a fresh attestation, which `maxAttestationAge` needs,
while one that cannot be reached grows stale; within the interval a repeated
`nyl attest` writes nothing.

### Reading a direct target

A target that declares `reconciler: {direct: {}}` has no Argo CD to read: Nyl
applied its Releases, and recorded what it did in the release state it keeps in
the target's Cluster (the `nyl.release.v1.<release>.<revision>` Secrets of
`nyl apply`, see the orchestration core contract's
[Direct reconciler](orchestration-core.md#direct-reconciler)). A
`DirectReader` maps that record and the live workloads into the same neutral
[`Reading`](#reading-argo-cd) Argo CD's readers produce, so the matching to a
publication, `accepted`, `healthy`, the details, coverage, and the teardown wait
are the shared rules, unchanged. The deployments are the target's Releases.

| Reading | A direct target |
| --- | --- |
| Source of truth | The release state of each Release, and `get` on the workload objects its latest `Deployed` revision tracks, through the kubeconfig the unit admits as `KUBECONFIG` and the Cluster's declared `spec.live.context`, never the current context |
| Running revision | The publication commit the latest revision recorded when the driver applied it (`Exact`). The shared matching then treats it exactly as an Argo CD revision: by the deployment directory's tree and recorded inputs, never by commit identity |
| `mixed` | The latest revision is `Failed` or still `Rendered`: an apply that failed or was interrupted after applying some resources. The last `Deployed` commit is reported beside it |
| `unmatched`, `unreadable` | A latest revision applied outside orchestration (`nyl apply`, `release rollback`) records no publication commit and is `unreadable`, with the fix to reconcile; revisions that cite another repository, branch, or prefix, or none, do not count at all |
| Health | Rollout status of Deployments, StatefulSets, DaemonSets, Jobs, and controller-less ReplicaSets, folded `Degraded`, `Progressing`, `Unknown`, `Healthy`, with the worst object in `reported`; a missing tracked workload is `Degraded` |
| `other-source`, `deleting`, `never-synced` | Do not arise: there is no configured source, and a revision exists only once recorded |
| Proof of absence | `Proof::Anchors`: a release list the credential may not make fails instead of filtering, and the anchor `release-state` shows the context reaches this target's cluster: a connection whose server was verified against the Cluster's declared destination server, or else at least one of the target's Releases having a revision of this publication (state exists only on a cluster Nyl applied to, and phase 1 of a teardown keeps it). A reading with no anchor is not a `missing` finding but unknown, and a teardown wait fails closed. The connection also refuses a context whose server differs from the declared destination |

In a reading that concludes absence, a deployment is *present* while a tracked
object that its `Delete` option deletes still exists, other than the
Namespaces holding the target's release state, which phase 1 defers to phase 2:
the teardown wait ends when the Releases' workloads are gone, exactly as for an
Application that disappears once its finalizer has deleted them.  Only
revisions whose recorded publication destination equals the target's count, in
either reading. Failure modes
are those of the Argo CD table: an unreachable API, a rejected or expired
credential, or a forbidden read leaves `nyl attest` `unknown` and the wait
`observation-failed`, naming the permission to grant; nothing falls back to the
process environment or `~/.kube/config`.
