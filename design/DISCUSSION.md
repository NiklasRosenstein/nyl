# Design discussion queue

Working list of design topics still to settle before implementation, taken one
at a time. When a topic is settled, its outcome goes into the contracts or
[ROADMAP.md](../ROADMAP.md) and the topic leaves this list; commit messages
carry the history. Topics are ordered by what the next ones depend on.

## Current

### 1. Cleaning up expired instances at `maxInstances`

At the instance limit, `state init --template` currently removes expired
instances first, inside the new pull request's job, with their waits,
destroys, and possible manual confirmations. Proposal: by default it refuses
with exit 2 and names the removable instances, leaving removal to the
scheduled fleet job; `--make-room` opts into removing them in the same job.
Open: which of the two is the default.

## Next

### 2. Preview pipeline commands and source commits

- Every push needs `state init --template … --param …` and then `reconcile`;
  the name `init` hides that the step also extends expiry.
- `reconcile -e pr-124` from a checkout of `main` renders pr-124's units at
  `main` and silently drops the pull request's changes.

Candidate directions: `reconcile -e <instance>` refuses a checkout that is not
the instance's recorded branch head unless given `--source`, and one command
that creates, extends, and reconciles an instance.

### 3. Reference project details

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

### 4. Scenario coverage by milestone

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
