# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Environments whose source moves along a promotion path

Settled so far:

- Every environment declares its source (`source: {ref: …}`), templates derive
  it from parameters, and runs never take it from the checkout.
- Prod declares `source: {promotion: dev-to-prod}`: it renders at the source
  commit dev ran when it produced the promoted evidence. Its first run is a
  promotion; before one exists, `reconcile -e prod` exits 2 and names
  `nyl promote dev-to-prod`.
- A PromotionPath always carries the source commit. Its value selectors carry
  only results that must not be rebuilt, such as image digests; Terraform
  source pins need no promotion, because prod runs `database` at the promoted
  commit.
- Prod avoids rebuilding by not selecting `web-image`; its target binds
  `image` through `fromPromotion`. Selectors support only `matchLabels`, so
  units carry one label per environment that selects them (`dev`, `prod`,
  `preview`). Nyl warns when an environment selects a unit that nothing in
  it consumes.

Open: which commit prod's own declarations are read from. The Environment
must come from the checkout, because it declares the source policy. Options:
(a) only the Environment comes from the checkout, so prod-only values change
without promotion; (b) the Environment, prod's DeploymentTarget, and its
Cluster come from the checkout, since they are prod's own, and only shared
definitions travel along the promotion path. Leaning to (b). Either way, the
declaration and the source commit must come from the same repository, and
`plan -e prod` shows both commits.

Afterwards: this diverges from the roadmap's M6, which promotes values while
every environment renders at the checkout's commit, so it needs a deliberate
review and a ROADMAP update before the contract changes.

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

- Hotfixes for environments whose source is promoted, for example a second
  source branch such as `release/prod` with its own environment and promotion
  path into prod, once promotion sources are settled.

- Promotion scenario (M6): staging or prod promoted from dev, extending the
  platform scenario.
