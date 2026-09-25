# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Structural templating of units and colocated resources

How far unit documents may be templated, and what that means for files that
hold several resources, such as all of one environment's resources in one
file.

Today's behavior, which units would follow:

- A file is split into YAML documents on the raw text before anything is
  rendered, and every document is discovered on its own. A template in one
  document can never affect another document's discovery.
- Each document must keep a literal envelope (`apiVersion`, `kind`,
  `metadata.name`, `metadata.labels`) that survives rendering unchanged.
  Below it, `ApplicationGroup` and `AppProjectDefinition` may use structural
  templating, including text that is not valid YAML before rendering.
- A templated document renders to exactly one document, or to none. Templates
  cannot span documents or generate several resources, for example with a loop
  around `---`.

Proposal for units:

- Unit kinds join the templatable kinds, with the same rules. Composition
  happens through selection and values, not through templates generating
  units.
- A unit document that renders to nothing is an error, not a silent
  deselection: `enabled: false` is the one way to switch a unit off, so plans
  always name the reason a unit leaves.
- Colocation is free on any axis. An environment file may hold its Cluster,
  DeploymentTarget, Environment, and environment-only units; units selected by
  several environments live wherever the project likes. Releases stay in their
  ApplicationGroup source directories, where the workload bundle loader finds
  them.
- The reference project shows the condensed form next to the expanded one.

Open: whether a document rendering to nothing should instead mean
`enabled: false` for units, as it effectively does for ApplicationGroups.

## Next

### 2. Leases in real CI

From the reference-scenario review:

- A job cancelled by the CI system (for example `cancel-in-progress`) keeps
  its lease until unit timeout plus grace, up to 70 minutes for OpenTofu, and
  no command breaks a lease: every later push exits 2 until then.
- A close job racing a running reconcile of the same instance exits 2 and can
  leak the preview.
- One busy instance fails the whole fleet job, `reconcile --template`.
- At `maxInstances`, `state init --template` runs another pull request's full
  teardown inside the new pull request's job, with that teardown's waits,
  credentials, and failure modes.

Candidate directions: release the lease on SIGTERM and checkpoint, a
`nyl lease break --reason` escape hatch, fleet runs that skip busy instances
and report them, and expiry removal only in scheduled jobs.

### 3. Preview pipeline commands and source commits

- Every push needs `state init --template … --param …` and then `reconcile`;
  the name `init` hides that the step also extends expiry.
- `reconcile -e pr-124` from a checkout of `main` renders pr-124's units at
  `main` and silently drops the pull request's changes.

Candidate directions: `reconcile -e <instance>` refuses a checkout that is not
the instance's recorded branch head unless given `--source`, and one command
that creates, extends, and reconciles an instance.

### 4. Reference project details

- Argo CD names and namespaces per preview instance: confirm that
  `values.nameSuffix` reaches the ApplicationGroup's `applicationNameTemplate`,
  project name, and namespace through today's templating, and show it.
- Static target `dev` carries `fromUnit` bindings, which M2 rejects outside
  orchestration, so `render-tree --target dev` fails without orchestration.
  Decide whether that is acceptable, or whether M5's "the same Releases still
  render with static inputs" needs a separate, orchestration-free target in the
  project.
- When `examples/platform/` is materialized (proposed: in M3, as schemas land).

### 5. Scenario coverage by milestone

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
