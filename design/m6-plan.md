# M6 plan: promotion paths

**Status:** plan for M6, written with its acceptance scenarios before any
feature code. It maps the roadmap items of
[M6](../nyl/book/src/lib/roadmaps/orchestration.mjs) onto a stack of pull
requests, lists the state and resource format changes M6 needs, and records
the decisions on what the [promotion contract](promotion.md) left open. The
user settled D1–D13, the format calls, and the deferral of target promotion
and the change gate to M11; the contract and the roadmap now say the same.
D14 is still open. Once M6 is
done, this note is history: the contract and the roadmap page describe what
was built.

## Acceptance scenarios

The exit criterion: *prod runs exactly the source commit and image digest dev
proved, with auditable lineage; stale, missing, or mixed source evidence
blocks promotion; a hotfix reaches prod through its own path, and a dev
promotion without the fix is refused until the fix is merged or superseded; a
value-only path promotes each value from what its consuming Application
runs.*

| Scenario | Proves | Clause | Tiers |
| --- | --- | --- | --- |
| [`reference/promotion`](../nyl/tests/scenarios/reference/promotion/scenario.yaml) (scenario 4) | Source commit and image digest from one recorded dev state, the record's lineage in state, `get promotions` and `status`; prod's own `requireDigest` approvals; missing evidence (no state, no `qa`) and aged evidence (`maxAttestationAge`); an unchanged re-promotion; `get states`; rollback with `--revision` and the recorded `--evidence published` override; a failing attestation blocking and `--ignore-attestation` recorded with its reason; artifact verification and `--no-verify-artifacts`; the record's `from` commits and `requested.reason`; scenario 1's step 21 | 1, 2 | 1, 2 (tail tier 1) |
| [`reference/hotfix`](../nyl/tests/scenarios/reference/hotfix/scenario.yaml) (scenario 5) | `prod-hotfix` and `hotfix-to-prod` with `paths`; path-less bindings follow the supplying record; `NYL-PROMOTE-SUPERSEDES` with the missing commits by subject; patch-equivalence (a cherry-picked fix commit counts, the uncherry-picked one stays missing, both cherry-picked pass without `--supersede`); a two-commit squash still refused; partial and reason-less `--supersede` refused, a range accepted and recorded per commit; merge back with a merge commit passes; the symmetric refusal; `release/prod` deleted once prod runs a commit reachable from `main` | 3 | 1, 2 |
| [`walkthroughs/stale-promotion-evidence`](../nyl/tests/scenarios/walkthroughs/stale-promotion-evidence/scenario.yaml) (strengthened) | Stale evidence (the M1 walkthrough), now also missing evidence, the record's values and evidence, `get promotion-candidates` naming both units, and prod publishing R1's digest | 1, 2 | 1 |
| [`walkthroughs/value-only-promotion`](../nyl/tests/scenarios/walkthroughs/value-only-promotion/scenario.yaml) | Each value from the publication its consuming Application runs (per-value `state`); the same mixed readings block the source path; an explicit older state passes on its recorded attestations; `unmatched` and `Degraded` block a value-only path; a binding with no record yet blocks | 2 (mixed), 4 | 1, fake publication |
| [`walkthroughs/promotion-record-source`](../nyl/tests/scenarios/walkthroughs/promotion-record-source/scenario.yaml) | `record: source`: the record is a commit on `main`; runs apply its sequence; an older checkout is refused (`NYL-PROMOTION-SEQUENCE-OLDER`); rollback is a new promotion | item 2 | 1, 2 |

Target sources, promotion into targets, `nyl attest --target`,
`changeGate: pullRequest`, and attesting an earlier state moved to M11 (see
[Deferred to M11](#deferred-to-m11)) and have no scenario here.

### How the pending scenarios stay green

The harness already has the convention: a scenario names the first milestone
whose harness runs it, and `scenario::REACHED` (M5) decides. Every scenario
above says `milestone: M6`, so today it is parsed and validated against the
format, its Git steps are replayed in a test world, and no runner runs it.

Raising `REACHED` is not enough on its own, though: `PENDING` is applied
only by `test_reached_scenarios_pass_in_tier_one`. The tier-2 runner
(`test_reached_scenarios_pass_in_tier_two`, a required CI job) and
`test_reached_scenarios_pass_in_tier_one_with_the_real_publication_driver`
filter on `milestone <= REACHED` alone, so `REACHED = M6` would run every
M6 scenario there and turn CI red. PR 2 therefore makes one function decide
which reached scenarios a runner runs, applies `PENDING` in all three
runners, and adds a test that a pending scenario runs in no runner. Each
later PR removes from `PENDING` the scenarios it turns green, and
`test_reached_scenarios_run_or_are_listed_as_pending` keeps the list honest.

The format rule is that a later milestone's step kinds join the format,
validation, and documentation together with the first scenario that uses
them, so this PR adds, format-only:

- step kinds `merge` (a merge commit, replayed already, because the Git
  replay test runs every scenario's Git steps) and `argocd` (tier 1: what the
  fake Argo CD reports per Release), both running from M6;
- the driver operation `verify-artifact`;
- argument placeholders `%STATE(<id>)%`, `%COMMIT(<revision>)%`, and
  `%PLAN-DIGEST(<env>/<unit>)%`, validated (known name, earlier step id,
  milestone at least M6);
- the `promotion` expectation's `record`, `sequence`, `from.environment`,
  `values` (`equals`, `state`), `evidence`, `superseded`, and
  `artifactsVerified`, with `sourceCommit: null` for value-only records.

The runner refuses `argocd` and `verify-artifact` steps with "not yet (M6)",
which only an M6 scenario can reach.

**Why they fail for the right reason.** With `REACHED` raised to M6
locally, all five fail at their first step that loads the project (`nyl
validate` in scenario 4, `nyl state init -e dev` in the others) with exit 1
and `Unsupported gitops.nyl/v1 kind "PromotionPath"`: the first M6 piece,
the resource itself, is missing, and nothing else stands in the way. After
PR 3 the first failure moves to the next missing piece (for example
`reconcile -e prod` exiting 1 with `NYL-SOURCE-UNAVAILABLE`, "takes its
source from a promotion, which is not supported yet", where the scenarios
expect 2 with `NYL-PROMOTION-MISSING`), and each PR's description records
where its remaining scenarios now stop.

### Harness defects found on the way

- `expect.promotion` is parsed but never checked: the runner ignores it, so
  the M1 walkthrough's `promotion` block proves nothing yet. PR 2 checks it.
- `commits` counts the refs of the step's `-e`, else dev's, so a
  `promote dev-to-prod` step counted dev's commits and its
  `commits: {desired: 0, observed: 0}` was vacuous. PR 2 counts the refs of
  the environment the path promotes into.
- `takes_json_output` lists the `get` forms that print JSON; PR 7 adds
  `states`, `promotion-candidates`, and `promotions`.

## Pull request stack

Each pull request is small enough to review in one sitting, keeps CI green,
updates its docs and `.changelog/_unreleased.toml`, ticks its roadmap items,
and removes from `PENDING` exactly the scenarios it turns green. "Depends on"
means it stacks on that branch.

| # | Branch | Scope | Turns green | Tests | Depends on |
| --- | --- | --- | --- | --- | --- |
| 1 | `m6/acceptance` | This note; the five scenarios; the format additions above; `reference-scenarios.md` scenarios 4 and 5 and the format; walkthrough prose in `orchestration-core.md` | Nothing; all M6 scenarios validated and pending by milestone | `test_validate_m6_steps_and_placeholders`; existing format and Git-replay tests over the new files | `develop` |
| 2 | `m6/harness` | `REACHED = M6` with every M6 scenario in `PENDING`, applied by all three runners (tier 1 with each publication driver, and tier 2) through one selection function; the runner resolves placeholders (`%STATE%` and `%OBSERVED%` from the commits a step with an `id` wrote, `%COMMIT%` in `project.git`, `%PLAN-DIGEST%` through `plan` in-process in tier 1 and through the binary in tier 2); checks `promotion` against the record where `record` says, including `from` commits, `requested`, and `ignoredAttestations`; counts a `promote` step's commits on its target environment and checks that every other state ref, the source environment's included, is unchanged; scripts `verify-artifact` | Nothing new; the harness can now fail an M6 scenario for its real reason | Runner unit tests for each check with hand-made records, like the existing `keep_refs` and `deploy` tests; a test that a `PENDING` scenario runs in no runner | 1 |
| 3 | `m6/resources` | `PromotionPath` (item 1) in `nyl-core::resources` with schema, catalogue entry, usage guide, and diagram; validation per the contract (source and target exist, selectors name selected units, pointers resolve against the kind's schema, value types fit every reading binding, `attestations` scopes declared); `nyl validate` and `plan` findings; `nyl get promotion-paths`; `PromotionRecord` state type with golden files; typed `source.fromPromotion.promoted` (one record shape, D4); validation rejects `changeGate: pullRequest` and a target in `from` or `to` with `NYL-PROMOTE-UNSUPPORTED`, naming M11; `walkthrough_resources_test` checks PromotionPath; `desired/promotions/` in the snapshot, in `state move`'s document copy, and in `state restore` (F1) | Scenario 4 step 1 and 13 reachable | Rule tests per validation; golden files for `PromotionRecord` v1 | 2 |
| 4 | `m6/external-attest` | Item 6 (attestations), external half: `nyl attest --name … --result pass\|fail [--reason] [--url]` for a unit's current receipt and for the environment's state, refusing undeclared names; `spec.attestations: [{name, from: external}]` accepted (retires `NYL-ATTESTATION-EXTERNAL-UNSUPPORTED` for dependents); attester recorded like an approver; `observed/attestations.yaml` read and written (F4); no `--state-revision`/`--revision` (D9) | `attested-evidence` keeps passing; scenario 4 steps 3–4 reachable | Shell tests for the scope and identity; rule tests for undeclared names | 2 |
| 5 | `m6/promotion-rules` | Item 2's decision core, pure in `nyl-orchestration::rules::promotion`: recorded states from desired/observed history by `Nyl-Run-Id`; the whole-source rule at `published` and `attested`, with `maxAttestationAge` and failing attestations blocking; state selection (newest, `--revision`, `--state-revision`, `--evidence published` as an override); value reading through the shared reference resolver; the `PromotionRecord` it would write; no effects | None alone | Property-style rule tests over synthetic histories: stale, missing, aged, failing, mixed, explicit older state | 3 |
| 6 | `m6/promote` | Items 1 and 2 shell: `nyl promote <path> [--value …] [--dry-run] [--reason]` under the target's lease, one desired commit with the record and a summary; `fromPromotion` sources (`path`, `record: state`) chosen by `rules::sources` with `sourceChosenBy: promotion` and `appliedPromotionSequence`; the `fromPromotion` binding provider for unit inputs and target `releaseInputs` in orchestration; `NYL-PROMOTION-MISSING` for a source or binding with no record; the driver capability `verify_artifact` (OciImage through `docker buildx imagetools inspect`, the fake image scripted), `verifyArtifacts` and `--no-verify-artifacts` | `promotion-record-source` still pending (D3); nothing else complete without inspection | Shell tests for the lease, the dry run, and the record commit; driver tests for verification | 4, 5 |
| 7 | `m6/inspect` | Items 4 and 8: `nyl get states -e`, `nyl get promotion-candidates <path>`, `nyl get promotions -e`, and `status` lineage (`promotion: {path, sequence, sourceCommit, from}`), one typed report each | `walkthroughs/stale-promotion-evidence`, `reference/promotion` | Report golden output; scenario steps | 6 |
| 8 | `m6/record-source` | Item 2: `record: source`, written as a commit on the entry branch (D3); validation of the block against the state it names; refusal of a lower sequence (`NYL-PROMOTION-SEQUENCE-OLDER`) | `walkthroughs/promotion-record-source` | Rule test for the sequence; shell test for the source commit | 7 |
| 9 | `m6/supersede` | Item 3: `source.fromPromotion.paths`, path-less bindings resolved from the supplying record, the per-value supersede check with `git cherry` containment (D5) in `nyl-git`, `--supersede` with ranges, qualification, and reasons, the record's `superseded` | `reference/hotfix` | Rule tests for containment (ancestor, cherry-pick, single-commit squash, multi-commit squash, merge, symmetric); `nyl-git` tests for patch-ids | 6 |
| 10 | `m6/fake-argocd` | Tier-1 Argo CD readings for `FakePublication`'s `attest`: the `argocd` step feeds the fake observer a neutral reading per target, evaluated by the shared matching, so `accepted`, `healthy`, and their details are the real rules' | Nothing alone | Fake observer tests; an inline scenario attesting `other-publication` | 2 |
| 11 | `m6/value-only` | Items 5 and 7: value-only paths take each value from the state its consumer runs (D6, D7); source paths gate on the newest publication attestations and block on mixed revisions; `coverage.applicationGroups`, `excludeApplications`, `requireApplications`; per-path `attestations` | `walkthroughs/value-only-promotion` | Rule tests over attestation details; coverage tests | 9, 10 |
| 15 | `m6/done` | Roadmap: every M6 item ticked, `done: true`, `Closes #…`; `REACHED` stays M6 with `PENDING` empty | — | The roadmap test | 11 |

There is no PR 12, 13, or 14: the change gate and target promotion moved to
M11, and this PR (PR 1) carries the roadmap change. The numbers stay so that
references to them keep their meaning.

Items to PRs: 1 → 3, 6; 2 → 5, 6, 8; 3 → 9; 4 → 7; 5 → 11; 6 → 4, 11;
7 → 11; 8 → 7 (the M6 items as renumbered by the roadmap change).

## Format changes

The M1 contract promised the PromotionRecord, its location, and the
environment record's promotion fields; several already exist in the v1 types.
The user decided F1 and F4: both files are added to state format `v1`
without a version bump. F5 and F6 belong to M11 and leave M6.

| # | Change | Kind | Where | Status in code | Decision |
| --- | --- | --- | --- | --- | --- |
| F1 | `PromotionRecord` kind at `desired/promotions/<path>.yaml` | **A new file in a shipped ref layout.** The contract promised it, but no release wrote it. What a Nyl without M6 does with it (verified in `nyl-state/src/git/mod.rs` and `nyl-orchestration/src/shell/state.rs`): the snapshot reads only `desired/environment.yaml` and `desired/units/` (`snapshot_of`), so it never sees the file; ordinary transitions, `hold`, `resume`, and `state forget` patch only the files they change beneath the location (`transition_commit`, `apply_changes`, `replace_beneath`), so the file survives them; `state delete` clears the location (`clear: true`), which removes it, as intended; `state move` keeps it when it imports history (both locations at the root path with the same ref layout, `import_history`) but **drops it** when it copies documents instead (a `path` or a different ref split), because the copy is built from the snapshot; `state restore` builds its changes from two snapshots (`restore_changes`), so it **neither restores nor removes** the file, leaving the current record next to a restored `appliedPromotionSequence`. An older Nyl also refuses a `fromPromotion` source, so it cannot run a promoted environment. The same holds for the current code until PR 3 adds the file to the snapshot, `state move`'s copy, and `state restore` | `nyl-core::orchestration::state`, golden files in `nyl-state`; snapshot, move, and restore in PR 3 | Listed in the state layout and kinds; no type | **Decided: additive within `v1`, no bump.** The release that writes it documents that an older Nyl keeps the file but cannot act on it and must not run `reconcile` in an environment that uses promotion; the docs also name the copying `state move` and `state restore` of an older Nyl, which do not carry or roll back the file |
| F2 | `EnvironmentRecord.appliedPromotionSequence`, `sourceChosenBy: {by: promotion, path, sequence}` | Fields already in the v1 type, never written so far | `state.rs` | Present | No |
| F3 | Environment `source.fromPromotion.promoted` typed as a `PromotionRecord` instead of any value | Source resource schema, not state; the field exists untyped and validation already reserves it | `resources/environment.rs` | Untyped `Value` | No, but D4 fixes its shape first |
| F4 | Environment-wide attestations in `observed/attestations.yaml`; external entries with `source: {external: {by, identity, source, url}}` | **`observed/attestations.yaml` is a new file in the observed layout**: the contract lists it, but the store reads only `observed/units/<unit>/attestations.yaml` (`attestations`, `unit_files`, `git/mod.rs:524`) and no transition writes it. Older Nyl treats it like F1: ignored by snapshots, kept by patching transitions, dropped by a copying `state move`, left alone by `state restore`. The entry's `source` is already an untyped value, so the external form needs no schema change | `nyl-state` snapshot and transitions, `state.rs` | Unit files only | **Decided: additive within `v1`, no bump**, with the same note in the docs of the release that writes it |
| F5 | The ownership index records each applied promotion record's `sequence` for a target that receives promotions | Would change `_nyl/index.json` (version 2) | — | Absent | **Deferred to M11** with promotion into targets; asked again there |
| F6 | `<prefix>/_nyl/attestations.yaml` on a publication branch (`nyl attest --target`) | Would add a reserved file to the published tree | — | Absent | **Deferred to M11** with `nyl attest --target`; asked again there |
| F7 | DeploymentTarget `spec.promotions` (M11) | Source resource schema, additive optional field | `nyl-render::resources::gitops` | Absent | No |
| F8 | `PromotionPath` resource | New source kind | `nyl-core::resources` | Absent | No |
| F9 | Reports: `PromoteReport`, `StatesReport`, `PromotionCandidatesReport`, `PromotionsReport`, `status`'s `promotion` | Machine output under its own `apiVersion`; additive | CLI reports | Absent | No |
| F10 | Transition commit summary for `operation: promote` (path, sequence, from) | Additive summary keys | `nyl-state` summary | Absent | No |

Nothing in M6 needs a change to `DesiredUnit`, `ObservedUnit`, `Run`, or
checkpoints: a value's source commits for the supersede check come from the
producing desired unit's `sourceRevision` and the commits resolution already
writes into each driver-declared source field (`Driver::source_fields`).
PR 9 must verify that for `OciImage` named contexts in other repositories;
if a commit is missing there, that becomes a `DesiredUnit` change and goes to
the user before it is made.

## Open decisions

The user accepted every recommendation of D1–D13; they are written into
`promotion.md` and `orchestration-core.md`, and the "Recommendation" column
is now the decision. D10 became the move to M11, with the code
`NYL-PROMOTE-UNSUPPORTED` for the forms M6 rejects (added to D1's list).
D14 is still open; scenario 4 assumes its recommendation.

| # | Question | Options | Decision (D14: recommendation) | PR |
| --- | --- | --- | --- | --- |
| D1 | Stable codes the contract does not name | — | `NYL-PROMOTION-MISSING` (exit 2: a `fromPromotion` source or binding with no record; names `nyl promote <path>`), `NYL-PROMOTE-ARTIFACT-MISSING` (exit 1), `NYL-PROMOTE-EVIDENCE-OVERRIDE` (warning, `--evidence published` below the path's level), `NYL-PROMOTE-SUPERSEDE-REASON` (exit 1, an entry with neither its own reason nor `--reason`), `NYL-PROMOTION-SEQUENCE-OLDER` (exit 1, a `record: source` block or `spec.promotions` record below the applied sequence), `NYL-PROMOTE-UNCHECKED` (warning, a value without a known source commit), and `NYL-PROMOTE-UNSUPPORTED` (exit 1, a form M6 does not support: `changeGate: pullRequest`, a target in `from` or `to`). Settled | 3, 6 |
| D2 | Promoting the state the path's record already names | Exit 0 writing nothing; or write a new record with a higher sequence | Exit 0, `outcome: unchanged`, when the chosen state and values equal the path's record *and* that record supplies every value; otherwise (another path wrote since) the promotion is a real one and the supersede check runs. Scenario 4 step 8 and scenario 5 step 10 depend on it Settled. | 6 |
| D3 | Where `record: source` and `spec.promotions` commit without a change gate | The entry worktree's branch; a configured branch; the Environment's `revision` | The branch the entry worktree has checked out, pushed fast-forward only; refuse a detached HEAD with the fix to check out a branch. A concurrent change is a refused push and a retry Settled. | 8 |
| D4 | The shape of the record outside state | The contract shows three shapes: `desired/promotions` (`values.<name>: {value, select, unit, …}`), `record: source` (`values: {webImage: <string>}`, attestations as strings), `spec.promotions` (`values: {webImage: {value, inputDigest}}`) | One `PromotionRecord` type everywhere, with the full `values.<name>` object and full attestation entries. Fix the two examples in `promotion.md` in PR 3 Settled. | 3 |
| D5 | What "contains" means for a record's source commit | Ancestor of the value's commit, or patch-equivalent per commit | Every commit reachable from the record's commit and not from the value's commit has a patch-equivalent in the value's history since their merge base (`git cherry <value> <record>` lists no `+`); merge commits are skipped as `git cherry` does. That is what makes "missing 2 commits: S1..c3d4" listable, what makes a two-commit squash fail and a single-commit squash pass, and what scenario 5 asserts Settled. | 9 |
| D6 | A value-only path when the publication's aggregate `accepted` fails only because Applications run different publications | The contract says both "a failing attestation blocks promotion of its state on every path" and "for a value-only path, Applications synced to different publications do not block" | For value-only paths, coverage is judged per covered Application from the attestation's details: a value is promotable when its consuming Application is accepted (and healthy, when required) for some recorded source publication. Only per-Application failures (`unmatched`, `mixed`, `missing`, `other-source`, unhealthy, …) of covered Applications block it. Say so in `promotion.md` under Health evidence Settled. | 11 |
| D7 | Which Application consumes a value selected from an artifact or output | Trace the publication's provenance to the Releases whose inputs came from it; or require `input` selectors on the publication unit | A value selected as `{unit: <publication>, input: /releases/<group>~1<release>/…}` is consumed by that Release's Application; an artifact or output selector is consumed by the producing unit itself (its newest receipt that meets the level). Tracing an artifact to the Releases that read it through provenance can follow later; the walkthrough uses `input` selectors Settled. | 11 |
| D8 | Source promotion at `attested` with publication units: newest passing state, or what runs now | "the newest state whose required attestations all passed" vs "what the newest recorded attestations show running, and mixed revisions block" | A publication unit's newest attestation decides what runs; mixed blocks without falling back to an older state that once passed, because that attestation no longer describes what runs. An explicitly chosen older state (`--state-revision`) uses the attestations recorded for it. Environment and non-publication unit attestations name a state or receipt, so for them the newest passing state wins (scenario 4 step 8) Settled. | 11 |
| D9 | `nyl attest --state-revision` / `--revision` for an earlier state | Write it into the same files, which hold the latest attestation per name for the *current* receipt or state, so an earlier state's entry would replace the current one and change what an existing file means; a new history file (another layout change, F4); or drop the flags | **Drop `--state-revision` and `--revision` from `nyl attest` in M6.** Earlier states keep the attestations recorded while they were current, which Git history of the observed ref already holds and promotion reads per state; attesting a past state can come later with a file of its own and its own format decision Settled. | 4 |
| D10 | `changeGate: pullRequest` | Forge API opening the pull request; push a branch and print the compare URL; defer | Settled: deferred to M11; until then validation rejects it with `NYL-PROMOTE-UNSUPPORTED` | 3 |
| D11 | `from` of a value-only record whose values come from different states | The newest state; the state of the first value; none | The newest state any value came from, with `state` on each value from another one (as the walkthrough asserts) Settled. | 11 |
| D12 | `--revision` on a fresh clone that lacks the commit | Fetch by id like recorded sources | Resolve against the source repository with the same fetch rules as a recorded source commit Settled. | 6 |
| D13 | Promoting an explicitly chosen older state at `attested` | `promotion.md` says both "Promoting an older state than the evidence level would select requires `--evidence published`, recorded as an override, for example to roll back" (Promotion paths, lines 198–200) and "An explicitly chosen state satisfies `attested` through attestations recorded for that state … Only a state without them needs `--evidence published`" (Promoting an environment's source, lines 264–270) | Narrow the first sentence to the second: an explicitly chosen state needs `--evidence published` only when the attestations recorded for it do not reach the level (including aged ones). Scenario 4 step 11 (aged `qa`, override needed) and `value-only-promotion` (R1 chosen with its recorded attestations, no override) follow the second reading Settled. | 5 |
| D14 | Exit of `nyl attest --name … --result fail` | The core contract's exit table lists `attest` in its own row (3 for a failing attestation) and again among the state operations (0 when done) | 3 with `NYL-ATTESTATION-FAILED`, as the driver form, so a CI job that records a failing check fails visibly; remove `attest` from the state-operations row. Scenario 4 step 12 assumes it | 4 |

## Contract problems (resolved)

Each was fixed in the contract with its decision, so the contract, the
scenarios, and this plan agree:

- The hotfix example now selects `hotfix: 'true'` (the image and the
  publication, on a target of its own) and says how a binding to a unit the
  hotfix environment does not run is satisfied.
- One PromotionRecord shape everywhere (D4); both examples outside state
  show it.
- Value-only coverage is judged per Application, and a failing aggregate
  that only reflects different publications does not block it (D6); state
  selection at `attested` with publication units is spelled out (D8).
- "Contains" is defined with `git cherry` (D5).
- The codes for a missing record, an older sequence, an override, a missing
  artifact, a reason-less supersede, an unchecked value, and an unsupported
  form are named (D1).
- Re-promotion of the recorded state is `unchanged` (D2); `record: source`
  commits to the entry worktree's branch (D3); `--revision` fetches like a
  recorded source (D12).
- The older-state sentence is narrowed to the explicit-selection rule (D13).
- `nyl attest --state-revision` is M11 (D9); `observed/attestations.yaml`
  and `desired/promotions/` are documented as additive within `v1`.

Still open: **D14**, the exit of `nyl attest --name … --result fail`, listed
twice in the core contract's exit table.

## Not covered by the scenarios

The acceptance scenarios cover the exit criterion. These M6 rules have no
scenario step yet; each gets one, or rule tests only, in the PR named:

- `nyl promote --value` (a subset promotion leaving other values at their
  lineage): no scenario path has two values that move separately; PR 6 adds
  an inline scenario or a shell test.
- A path's `attestations` field (per-unit and environment overrides), and
  `NYL-ATTESTATION-UNDECLARED` for a path: PR 3 and PR 11, rule tests.
- `coverage.applicationGroups`, `excludeApplications`, `requireApplications`:
  PR 11.
- `NYL-PROMOTE-UNCHECKED` (a value without a known source commit) and
  repository-qualified `--supersede web:S1..c3d4`: PR 9, rule tests.
- A target environment that rebuilds a unit its `fromPromotion` bindings
  replace (the warning and the "rebuilt" record): PR 6.
- `nyl promote` while the target's lease is held (exit 2): PR 6, shell test.
- Renaming a source unit breaking future promotions while recorded lineage
  stays valid: PR 3 (validation) and PR 5.

## Deferred to M11

The user deferred these to a new stage, M11 "Promotion for non-orchestrated
targets and change gates", which needs M6. This PR made the roadmap change,
in `ROADMAP.md` and `nyl/book/src/lib/roadmaps/orchestration.mjs` together:

- M11 holds the former M6 items 6 (promotion from a target's published
  inputs) and 7 (promotion into a target), plus `nyl attest --target` with
  `<prefix>/_nyl/attestations.yaml` and gating on it, `changeGate:
  pullRequest`, and attesting an earlier state with `nyl attest
  --state-revision`/`--revision` (D9).
- M6 item 1 records each promotion directly and rejects
  `changeGate: pullRequest`; the former item 8, now 6 (attestations), covers external
  attestations for units and environments, not targets; the former item 9, now 7, gates
  promotion from an environment, the one source kind M6 has.
- The next step's list and outro name M11 for what moved.
- ROADMAP.md's "Promotion paths" section says which half is M6 and why the
  rest is M11.
- The format calls F5 and F6 move with their items and are asked again in
  M11.

Validation in M6 rejects `changeGate: pullRequest` and a target in `from` or
`to` with `NYL-PROMOTE-UNSUPPORTED` (PR 3), so a project cannot rely on a
form that silently does nothing.

The coverage options (`applicationGroups`, `excludeApplications`,
`requireApplications`) stay in M6 (item 7) but can follow PR 11 if it grows.
