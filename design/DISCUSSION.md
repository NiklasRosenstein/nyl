# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Environments whose source moves along a promotion path

Settled so far: every environment declares its source (`source: {ref: …}`),
templates derive it from parameters, and runs never take it from the checkout.

Open: prod should not follow `main`. A drastic change to a unit's shape must
reach prod only after dev proved it, so prod's source commit itself moves
along its promotion path. Proposal:

- `source: {promotion: dev-to-prod}`: prod's S is the source commit recorded
  by the latest promotion on that path, which is the commit dev ran when it
  produced the promoted evidence.
- A PromotionPath then always carries the source commit, and its value
  selectors keep deciding which results are reused rather than rebuilt, such
  as dev's image digest instead of a new build at the promoted commit.
- Consistency: the promoted source commit and the promoted values come from
  the same dev state, as the roadmap already requires for values.

This diverges from the roadmap's M6, which promotes values while every
environment renders at the checkout's commit, so it needs the deliberate
review AGENTS.md asks for and a ROADMAP update. Questions to settle: how the
first prod run gets a source before any promotion, whether values without a
selector are rendered from the promoted commit or rejected, and how a prod
hotfix bypasses dev.

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

## Later

- Promotion scenario (M6): staging or prod promoted from dev, extending the
  platform scenario.
