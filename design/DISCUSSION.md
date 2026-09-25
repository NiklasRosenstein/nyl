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

- Both modes are valid configurations: a prod that selects `web-image`
  rebuilds it from the promoted commit; a prod that does not binds dev's
  proven digest through `fromPromotion`. Nothing prevents either.
- The current source is authoritative except where a revision is needed to
  build or produce something. Placement resources are read from the current
  source: Environment, DeploymentTarget (including its `releaseInputs`),
  Cluster, ArgoCDInstance, GitRepository. Definitions are read at the promoted
  commit S: units, ApplicationGroups, Releases, AppProjectDefinitions, charts,
  and everything units build from, such as Terraform modules, Docker contexts,
  and Command files. Prod-only changes such as replicas or a database class
  apply directly; changes to what the code does travel through dev.
- Prod-only changes turn knobs the definitions at S already expose: a
  Release input bound on prod's DeploymentTarget
  (`releaseInputs: {web/web: {replicas: {value: 10}}}`), or an Environment
  value a unit spec at S reads (`instance_class: '{{ values.dbClass }}'`).
  Adding a knob is a definition change and travels through dev. A placement
  file that binds something the definitions at S do not declare fails before
  anything executes, naming the promoted commit.
- All definitions of one run come from the same S, because a unit's
  declaration and the source it builds from must match (a new variable in the
  `database` unit must meet the module that declares it). `plan -e prod` shows
  both commits.

Next step: this diverges from the roadmap's M6, which promotes values while
every environment renders at the checkout's commit, so it needs a deliberate
review of the promotion contract and a ROADMAP update, then the contract
changes (Source section, PromotionPath, reference project's prod).

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
