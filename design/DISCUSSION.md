# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Reference project details

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

## Next

### 2. Scenario coverage by milestone

- M3's Command stand-ins cannot express `bind: plan` approvals or teardown
  steps, so the platform scenario's approval and teardown steps cannot pass in
  M3.
- M4's slice of the platform scenario includes the publication unit, an M5
  item.
- Tier 2 skips when tools are missing, so "passes with the real tools" can pass
  without running; decide where tier 2 runs as a required check.

## Later

- Hotfixes for environments whose source is promoted, for example a second
  source branch such as `release/prod` with its own environment and promotion
  path into prod, once promotion sources are settled.

- Promotion scenario (M6): staging or prod promoted from dev, extending the
  platform scenario.
