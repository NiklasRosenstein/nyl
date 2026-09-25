# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Promotion of environment sources

The environment `source` field is settled in the contract's Source section:
omitted (entry worktree), `revision` with an optional `commit` lock, or
`fromPromotion`; one worktree per run; state records S. The review that
rejected reading placement and definitions from different commits is the
reason for one worktree per run.

Settled for promotion, to write into the roadmap's promotion section and M6
after a deliberate review:

- `nyl promote` moves the pin of every promotable form: it writes the
  `commit` lock for `revision` and `commit` (a pull request under
  `changeGate: pullRequest`), opens a pull request merging S into `revision`
  for `revision` alone, and records S in the PromotionRecord for
  `fromPromotion`. An omitted source is not a promotion target.
- The unit of promotion is one recorded dev state, whose transition commits
  tie together the source commit, the desired documents, and the receipts
  with their artifacts. The PromotionRecord records that state's desired and
  observed commits, its source commit, and every value taken from it, so an
  environment's promoted source and its `fromPromotion` bindings (such as
  dev's image digest) always come from the same dev run.
- `nyl promote` without flags takes the newest dev state that meets the
  path's evidence level. `--revision <source commit>` resolves to the newest
  dev state with that source whose promoted units had current receipts;
  `--state-revision <dev desired commit>` names a state exactly. An older
  state needs `--evidence published`, recorded as an override, because dev no
  longer runs it.
- Evidence for a promoted source: every unit the target environment also
  selects has a current receipt in that dev state, and at `healthy` every
  covered Application runs that state's publication. Mixed revisions block,
  unlike value-only promotion today.
- Prod may select `web-image` and rebuild it at S, or bind dev's digest
  through `fromPromotion`. When it rebuilds, the PromotionRecord says the
  result was rebuilt rather than claiming dev's evidence for it, image tags
  include the environment (`nyl-<env>-<key>`), and the warning about unused
  units fires only for units whose artifacts `fromPromotion` bindings replace.
- Prod-only changes, such as replicas, reach prod with the next promotion of a
  commit that contains them; hotfixes that cannot wait use a `revision` such
  as `release/prod`.

## Next

### 2. Reference project details

- Argo CD names and namespaces per preview instance: confirm that
  `values.nameSuffix` reaches the ApplicationGroup's `applicationNameTemplate`,
  project name, and namespace through today's templating, and show it.
- Static target `dev` carries `fromUnit` bindings, which M2 rejects outside
  orchestration, so `render-tree --target dev` fails without orchestration.
  Decide whether that is acceptable, or whether M5's "the same Releases still
  render with static inputs" needs a separate, orchestration-free target in the
  project.
- When `examples/platform/` is materialized (proposed: in M3, as schemas land),
  and a condensed variant that colocates each environment's resources in one
  file.

### 3. Scenario coverage by milestone

- M3's Command stand-ins cannot express `bind: plan` approvals or teardown
  steps, so the platform scenario's approval and teardown steps cannot pass in
  M3.
- M4's slice of the platform scenario includes the publication unit, an M5
  item.
- Tier 2 skips when tools are missing, so "passes with the real tools" can pass
  without running; decide where tier 2 runs as a required check.

### Direct image builds

An `OciImage` unit is a complete description of an image build, so it could
serve as a build configuration on its own, the way `nyl render` uses Releases
without orchestration. Sketch: `nyl build <unit> [-e <env>] [--push | --load]
[--platform …]` renders the unit (with an environment's values and read-only
references to its state when `-e` is given, defaults otherwise), runs the same
buildx invocation, and writes no state. Open: whether `fromUnit` build
arguments and image contexts without `-e` fail or take `--context`/`--build-arg`
overrides, and whether it belongs in M4.

## Later

- Hotfixes for environments whose source is promoted, for example a second
  source branch such as `release/prod` with its own environment and promotion
  path into prod, once promotion sources are settled.

- Promotion scenario (M6): staging or prod promoted from dev, extending the
  platform scenario.
