# M6 plan: promotion paths

**Status:** plan for M6, written with its acceptance scenarios before any
feature code. It maps the ten roadmap items of
[M6](../nyl/book/src/lib/roadmaps/orchestration.mjs) onto a stack of pull
requests, lists the state and resource format changes M6 needs, and names
the decisions the [promotion contract](promotion.md) leaves open. Once M6 is
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

Items 6 and 7 (target sources and targets as promotion targets) and the
`changeGate: pullRequest` half of item 1 have no scenario here; see
[Deferrable items](#deferrable-items).

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
| 3 | `m6/resources` | `PromotionPath` (item 1) in `nyl-core::resources` with schema, catalogue entry, usage guide, and diagram; validation per the contract (source and target exist, selectors name selected units, pointers resolve against the kind's schema, value types fit every reading binding, `attestations` scopes declared); `nyl validate` and `plan` findings; `nyl get promotion-paths`; `PromotionRecord` state type with golden files; typed `source.fromPromotion.promoted`; `walkthrough_resources_test` checks PromotionPath; `desired/promotions/` in the snapshot, in `state move`'s document copy, and in `state restore` (F1) | Scenario 4 step 1 and 13 reachable | Rule tests per validation; golden files for `PromotionRecord` v1 | 2 |
| 4 | `m6/external-attest` | Item 8, external half: `nyl attest --name … --result pass\|fail [--reason] [--url]` for a unit's current receipt and for the environment's state, refusing undeclared names; `spec.attestations: [{name, from: external}]` accepted (retires `NYL-ATTESTATION-EXTERNAL-UNSUPPORTED` for dependents); attester recorded like an approver; `observed/attestations.yaml` read and written (F4); no `--state-revision`/`--revision` (D9) | `attested-evidence` keeps passing; scenario 4 steps 3–4 reachable | Shell tests for the scope and identity; rule tests for undeclared names | 2 |
| 5 | `m6/promotion-rules` | Item 2's decision core, pure in `nyl-orchestration::rules::promotion`: recorded states from desired/observed history by `Nyl-Run-Id`; the whole-source rule at `published` and `attested`, with `maxAttestationAge` and failing attestations blocking; state selection (newest, `--revision`, `--state-revision`, `--evidence published` as an override); value reading through the shared reference resolver; the `PromotionRecord` it would write; no effects | None alone | Property-style rule tests over synthetic histories: stale, missing, aged, failing, mixed, explicit older state | 3 |
| 6 | `m6/promote` | Items 1 and 2 shell: `nyl promote <path> [--value …] [--dry-run] [--reason]` under the target's lease, one desired commit with the record and a summary; `fromPromotion` sources (`path`, `record: state`) chosen by `rules::sources` with `sourceChosenBy: promotion` and `appliedPromotionSequence`; the `fromPromotion` binding provider for unit inputs and target `releaseInputs` in orchestration; `NYL-PROMOTION-MISSING` for a source or binding with no record; the driver capability `verify_artifact` (OciImage through `docker buildx imagetools inspect`, the fake image scripted), `verifyArtifacts` and `--no-verify-artifacts` | `promotion-record-source` still pending (D3); nothing else complete without inspection | Shell tests for the lease, the dry run, and the record commit; driver tests for verification | 4, 5 |
| 7 | `m6/inspect` | Item 4 and 10: `nyl get states -e`, `nyl get promotion-candidates <path>`, `nyl get promotions -e`, and `status` lineage (`promotion: {path, sequence, sourceCommit, from}`), one typed report each | `walkthroughs/stale-promotion-evidence`, `reference/promotion` | Report golden output; scenario steps | 6 |
| 8 | `m6/record-source` | Item 2: `record: source`, written as a commit on the entry branch (D3); validation of the block against the state it names; refusal of a lower sequence (`NYL-PROMOTION-SEQUENCE-OLDER`) | `walkthroughs/promotion-record-source` | Rule test for the sequence; shell test for the source commit | 7 |
| 9 | `m6/supersede` | Item 3: `source.fromPromotion.paths`, path-less bindings resolved from the supplying record, the per-value supersede check with `git cherry` containment (D5) in `nyl-git`, `--supersede` with ranges, qualification, and reasons, the record's `superseded` | `reference/hotfix` | Rule tests for containment (ancestor, cherry-pick, single-commit squash, multi-commit squash, merge, symmetric); `nyl-git` tests for patch-ids | 6 |
| 10 | `m6/fake-argocd` | Tier-1 Argo CD readings for `FakePublication`'s `attest`: the `argocd` step feeds the fake observer a neutral reading per target, evaluated by the shared matching, so `accepted`, `healthy`, and their details are the real rules' | Nothing alone | Fake observer tests; an inline scenario attesting `other-publication` | 2 |
| 11 | `m6/value-only` | Items 5 and 9: value-only paths take each value from the state its consumer runs (D6, D7); source paths gate on the newest publication attestations and block on mixed revisions; `coverage.applicationGroups`, `excludeApplications`, `requireApplications`; per-path `attestations` | `walkthroughs/value-only-promotion` | Rule tests over attestation details; coverage tests | 9, 10 |
| 12 | `m6/change-gate` | Item 1, rest: `changeGate: pullRequest` (D10) | Inline scenario with a fake forge | Forge adapter tests | 9 |
| 13 | `m6/target-source` | Item 6: `from: {target}`: publication commits, value recovery from provenance verified against `@input` digests, `nyl attest --target` and `<prefix>/_nyl/attestations.yaml` | A scenario of its own, written in that PR | Rule tests for recovery and digest checks | 11 |
| 14 | `m6/into-target` | Item 7: `to: {target}`, `spec.promotions`, resolution in tree and direct commands, the ownership index's applied sequence (F5) | A scenario of its own | Tree tests for the refusal of an older sequence | 13 |
| 15 | `m6/done` | Roadmap: every item that stays in M6 ticked, `done: true`, `Closes #…`; `REACHED` stays M6 with `PENDING` empty | — | The roadmap test | 11 and the roadmap change of [Deferrable items](#deferrable-items); 12–14 only if the user keeps them in M6 |

Items to PRs: 1 → 3, 6, 12; 2 → 5, 6, 8; 3 → 9; 4 → 7; 5 → 11; 6 → 13;
7 → 14; 8 → 4, 11; 9 → 11; 10 → 7.

## Format changes

The M1 contract promised the PromotionRecord, its location, and the
environment record's promotion fields; several already exist in the v1 types.

| # | Change | Kind | Where | Status in code | Needs the user's call |
| --- | --- | --- | --- | --- | --- |
| F1 | `PromotionRecord` kind at `desired/promotions/<path>.yaml` | **A new file in a shipped ref layout.** The contract promised it, but no release wrote it. What a Nyl without M6 does with it (verified in `nyl-state/src/git/mod.rs` and `nyl-orchestration/src/shell/state.rs`): the snapshot reads only `desired/environment.yaml` and `desired/units/` (`snapshot_of`), so it never sees the file; ordinary transitions, `hold`, `resume`, and `state forget` patch only the files they change beneath the location (`transition_commit`, `apply_changes`, `replace_beneath`), so the file survives them; `state delete` clears the location (`clear: true`), which removes it, as intended; `state move` keeps it when it imports history (both locations at the root path with the same ref layout, `import_history`) but **drops it** when it copies documents instead (a `path` or a different ref split), because the copy is built from the snapshot; `state restore` builds its changes from two snapshots (`restore_changes`), so it **neither restores nor removes** the file, leaving the current record next to a restored `appliedPromotionSequence`. An older Nyl also refuses a `fromPromotion` source, so it cannot run a promoted environment. The same holds for the current code until PR 3 adds the file to the snapshot, `state move`'s copy, and `state restore` | `nyl-core::orchestration::state`, golden files in `nyl-state`; snapshot, move, and restore in PR 3 | Listed in the state layout and kinds; no type | **Yes**: additive for readers, but `state move` (copy form) and `state restore` by an older Nyl lose or desynchronize records, so mixed versions sharing state need the user's bump-or-break call |
| F2 | `EnvironmentRecord.appliedPromotionSequence`, `sourceChosenBy: {by: promotion, path, sequence}` | Fields already in the v1 type, never written so far | `state.rs` | Present | No |
| F3 | Environment `source.fromPromotion.promoted` typed as a `PromotionRecord` instead of any value | Source resource schema, not state; the field exists untyped and validation already reserves it | `resources/environment.rs` | Untyped `Value` | No, but D4 fixes its shape first |
| F4 | Environment-wide attestations in `observed/attestations.yaml`; external entries with `source: {external: {by, identity, source, url}}` | **`observed/attestations.yaml` is a new file in the observed layout**: the contract lists it, but the store reads only `observed/units/<unit>/attestations.yaml` (`attestations`, `unit_files`, `git/mod.rs:524`) and no transition writes it. Older Nyl treats it like F1: ignored by snapshots, kept by patching transitions, dropped by a copying `state move`, left alone by `state restore`. The entry's `source` is already an untyped value, so the external form needs no schema change | `nyl-state` snapshot and transitions, `state.rs` | Unit files only | **Yes**, as for F1 |
| F5 | The ownership index records each applied promotion record's `sequence` for a target that receives promotions in `spec.promotions` (item 7) | **Changes an existing format**: `_nyl/index.json`, format version 2. M2 added input digests under reserved `@` keys of `inputs` without a version bump, by the user's decision; the same route would add `@promotions/<path>` | `nyl-render::gitops` ownership index | Absent | **Yes: bump or break** |
| F6 | `<prefix>/_nyl/attestations.yaml` on a publication branch (`nyl attest --target`) | New reserved file in the published tree; older Nyl sees an unowned file, which reconciliation preserves, so it degrades safely; but the reserved-path rule changes what `publish-tree` treats as unowned | Publication layout | Absent | **Yes**, as a layout change of the published tree, though additive in effect |
| F7 | DeploymentTarget `spec.promotions` | Source resource schema, additive optional field | `nyl-render::resources::gitops` | Absent | No |
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

Each needs an answer before the PR named; the recommendation is what the
scenarios assume.

| # | Question | Options | Recommendation | PR |
| --- | --- | --- | --- | --- |
| D1 | Stable codes the contract does not name | — | `NYL-PROMOTION-MISSING` (exit 2: a `fromPromotion` source or binding with no record; names `nyl promote <path>`), `NYL-PROMOTE-ARTIFACT-MISSING` (exit 1), `NYL-PROMOTE-EVIDENCE-OVERRIDE` (warning, `--evidence published` below the path's level), `NYL-PROMOTE-SUPERSEDE-REASON` (exit 1, an entry with neither its own reason nor `--reason`), `NYL-PROMOTION-SEQUENCE-OLDER` (exit 1, a `record: source` block or `spec.promotions` record below the applied sequence), `NYL-PROMOTE-UNCHECKED` (warning, a value without a known source commit). The scenarios use these names | 3, 6 |
| D2 | Promoting the state the path's record already names | Exit 0 writing nothing; or write a new record with a higher sequence | Exit 0, `outcome: unchanged`, when the chosen state and values equal the path's record *and* that record supplies every value; otherwise (another path wrote since) the promotion is a real one and the supersede check runs. Scenario 4 step 8 and scenario 5 step 10 depend on it | 6 |
| D3 | Where `record: source` and `spec.promotions` commit without a change gate | The entry worktree's branch; a configured branch; the Environment's `revision` | The branch the entry worktree has checked out, pushed fast-forward only; refuse a detached HEAD with the fix to check out a branch. A concurrent change is a refused push and a retry | 8 |
| D4 | The shape of the record outside state | The contract shows three shapes: `desired/promotions` (`values.<name>: {value, select, unit, …}`), `record: source` (`values: {webImage: <string>}`, attestations as strings), `spec.promotions` (`values: {webImage: {value, inputDigest}}`) | One `PromotionRecord` type everywhere, with the full `values.<name>` object and full attestation entries. Fix the two examples in `promotion.md` in PR 3 | 3 |
| D5 | What "contains" means for a record's source commit | Ancestor of the value's commit, or patch-equivalent per commit | Every commit reachable from the record's commit and not from the value's commit has a patch-equivalent in the value's history since their merge base (`git cherry <value> <record>` lists no `+`); merge commits are skipped as `git cherry` does. That is what makes "missing 2 commits: S1..c3d4" listable, what makes a two-commit squash fail and a single-commit squash pass, and what scenario 5 asserts | 9 |
| D6 | A value-only path when the publication's aggregate `accepted` fails only because Applications run different publications | The contract says both "a failing attestation blocks promotion of its state on every path" and "for a value-only path, Applications synced to different publications do not block" | For value-only paths, coverage is judged per covered Application from the attestation's details: a value is promotable when its consuming Application is accepted (and healthy, when required) for some recorded source publication. Only per-Application failures (`unmatched`, `mixed`, `missing`, `other-source`, unhealthy, …) of covered Applications block it. Say so in `promotion.md` under Health evidence | 11 |
| D7 | Which Application consumes a value selected from an artifact or output | Trace the publication's provenance to the Releases whose inputs came from it; or require `input` selectors on the publication unit | A value selected as `{unit: <publication>, input: /releases/<group>~1<release>/…}` is consumed by that Release's Application; an artifact or output selector is consumed by the producing unit itself (its newest receipt that meets the level). Tracing an artifact to the Releases that read it through provenance can follow later; the walkthrough uses `input` selectors | 11 |
| D8 | Source promotion at `attested` with publication units: newest passing state, or what runs now | "the newest state whose required attestations all passed" vs "what the newest recorded attestations show running, and mixed revisions block" | A publication unit's newest attestation decides what runs; mixed blocks without falling back to an older state that once passed, because that attestation no longer describes what runs. An explicitly chosen older state (`--state-revision`) uses the attestations recorded for it. Environment and non-publication unit attestations name a state or receipt, so for them the newest passing state wins (scenario 4 step 8) | 11 |
| D9 | `nyl attest --state-revision` / `--revision` for an earlier state | Write it into the same files, which hold the latest attestation per name for the *current* receipt or state, so an earlier state's entry would replace the current one and change what an existing file means; a new history file (another layout change, F4); or drop the flags | **Drop `--state-revision` and `--revision` from `nyl attest` in M6.** Earlier states keep the attestations recorded while they were current, which Git history of the observed ref already holds and promotion reads per state; attesting a past state can come later with a file of its own and its own format decision | 4 |
| D10 | `changeGate: pullRequest` | Forge API (GitHub first) opening the pull request; or push a branch and print the compare URL; or defer | Defer to its own PR at the end of the stack: it needs a forge adapter, credentials, and idempotent updates, none of which the rest needs. Until then validation rejects `pullRequest` with a code naming the milestone | 12 |
| D11 | `from` of a value-only record whose values come from different states | The newest state; the state of the first value; none | The newest state any value came from, with `state` on each value from another one (as the walkthrough asserts) | 11 |
| D12 | `--revision` on a fresh clone that lacks the commit | Fetch by id like recorded sources | Resolve against the source repository with the same fetch rules as a recorded source commit | 6 |
| D13 | Promoting an explicitly chosen older state at `attested` | `promotion.md` says both "Promoting an older state than the evidence level would select requires `--evidence published`, recorded as an override, for example to roll back" (Promotion paths, lines 198–200) and "An explicitly chosen state satisfies `attested` through attestations recorded for that state … Only a state without them needs `--evidence published`" (Promoting an environment's source, lines 264–270) | Narrow the first sentence to the second: an explicitly chosen state needs `--evidence published` only when the attestations recorded for it do not reach the level (including aged ones). Scenario 4 step 11 (aged `qa`, override needed) and `value-only-promotion` (R1 chosen with its recorded attestations, no override) follow the second reading | 5 |
| D14 | Exit of `nyl attest --name … --result fail` | The core contract's exit table lists `attest` in its own row (3 for a failing attestation) and again among the state operations (0 when done) | 3 with `NYL-ATTESTATION-FAILED`, as the driver form, so a CI job that records a failing check fails visibly; remove `attest` from the state-operations row. Scenario 4 step 12 assumes it | 4 |

## Contract problems

- **Hotfix example.** `promotion.md#hotfixes` gives `prod-hotfix` prod's
  selector (`prod: 'true'`), but prod selects no `web-image`, so
  `hotfix-to-prod`'s selector names a unit the source does not run and fails
  validation. Scenario 5 selects `hotfix: 'true'` (`web-image`, `kubernetes`)
  and gives the hotfix target a static database host. Fix the example in
  PR 9.
- **Record shapes** disagree between the three locations (D4).
- **Attestation blocking** contradicts value-only coverage (D6), and
  state selection at `attested` reads two ways (D8).
- **"Contains"** is defined for a commit, but the refusal message lists a
  range of missing commits (D5).
- **`reconcile -e prod` before the first promotion** "exits 2 naming `nyl
  promote`" with no code (D1).
- **Re-promotion** of an already promoted state is not specified (D2).
- **Older states at `attested`**: `promotion.md` lines 198–200 require
  `--evidence published` for any state older than the level would select,
  while lines 264–270 let an explicitly chosen state pass on the
  attestations recorded for it (D13). The scenarios follow the second.
- **`attest` exit** is listed twice in the core contract's exit table (D14).
- **`record: source` without a gate** does not say which branch it commits to
  (D3).
- **`nyl attest --state-revision`** has no storage that keeps the current
  state's entry intact (D9).
- **Hotfix target `databaseHost`.** The contract's hotfix environment runs no
  database; any binding prod's target reads `fromUnit` must be satisfied
  differently on the hotfix target. Worth one sentence in the contract.
- **Reference scenarios coverage** promised "a value-only path" inside the
  fourth scenario; it is the `value-only-promotion` walkthrough instead,
  because it needs scripted Argo CD readings that tier 2 cannot give. The
  coverage table says so now.

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
- Items 6 and 7 and `changeGate: pullRequest`, if kept in M6.

## Deferrable items

- **Item 7, promotion into a target** (PR 14): touches the ownership index
  (F5, a format decision), `publish-tree`, `render-tree`, `diff-tree`, and the
  direct commands, outside orchestration, with a no-lease concurrency model.
  Nothing in the exit criterion needs it. Recommend splitting it into a
  follow-up stage after M6, or at least landing it last.
- **Item 6, target sources** (PR 13): value recovery from provenance (carried
  state files, locked `fromGit` blobs, bindings at a recorded commit) with
  digest verification, and `nyl attest --target` writing into publication
  branches (F6). Not in the exit criterion. Recommend deferring together with
  item 7, since both serve non-orchestrated targets.
- **`changeGate: pullRequest`** (PR 12, D10): forge integration, separable
  and not in the exit criterion.
- **Coverage options** (`applicationGroups`, `excludeApplications`,
  `requireApplications`): part of item 9, small, but they can follow PR 11 if
  it grows.

**If the user decides to defer** items 6 and 7 and the change gate, one
roadmap change lands before PR 15, in `ROADMAP.md` and
`nyl/book/src/lib/roadmaps/orchestration.mjs` together:

- moves items 6 and 7 and `changeGate: pullRequest` to a follow-up stage
  (for example "M6b: promotion for non-orchestrated targets and change
  gates", needing M6), with its own exit criterion;
- rewrites item 1 without "and an optional pull-request change gate";
- rewrites item 8 without "`nyl attest` for … targets" (it keeps
  environments), and without `--state-revision` if D9 drops it;
- rewrites item 9 from "for both PromotionPath sources" to environment
  sources;
- updates the ROADMAP.md "Promotion paths (M6)" section, which lists target
  sources and promotion into targets as M6.

PR 15 then depends on that change and on PR 11, not on PRs 12–14. If the
user keeps them in M6, PRs 12–14 stay in the stack and PR 15 depends on all.
