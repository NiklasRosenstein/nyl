# Contract walkthroughs

Each directory is one walkthrough from the
[orchestration core contract](../../../../design/orchestration-core.md#walkthroughs),
written in the scenario format that the
[reference scenarios](../../../../design/reference-scenarios.md#scenario-files)
define. `_project/` is the project they share unless a scenario brings its own.
Its units declare the `timeout` of the real kinds the fake kinds stand in for
(60m for OpenTofu and OciImage, 10m for KubernetesPublication), so lease
deadlines match the walkthroughs' prose. The
[crash-injection scenarios](../recovery/README.md) in `recovery/` use it too.

| Scenario | Walkthrough | Runs from |
| --- | --- | --- |
| [`dependency-wave`](dependency-wave/scenario.yaml) | Successful dependency wave | M3 |
| [`unit-jobs`](unit-jobs/scenario.yaml) | Separate unit-scoped CI invocations | M3 |
| [`ci-admission`](ci-admission/scenario.yaml) | Superseded worker retries, idempotent dispatch, and authorized rollback | M8 |
| [`ci-lifecycle`](ci-lifecycle/scenario.yaml) | Leaving units in planning and a separate lifecycle job | M8 |
| [`unavailable-upstream-output`](unavailable-upstream-output/scenario.yaml) | Unavailable upstream output | M3 |
| [`effects-without-receipt`](effects-without-receipt/scenario.yaml) | Effects without a receipt | M3 |
| [`competing-runners`](competing-runners/scenario.yaml) | Competing runners, and a lease break: `verify` refuses, `recover` imports the dead run once | M3 |
| [`deletion`](deletion/scenario.yaml) | Deletion | M3 |
| [`decommission`](decommission/scenario.yaml) | Deletion with the operator commands: `teardown` (a replace, `--hold`, a retained tombstone), a teardown requested while held, a kind change, and `state delete --teardown` next to an environment sharing its refs | M3 |
| [`held-uncertain`](held-uncertain/scenario.yaml) | Holds on uncertain units: a consumer reconciles against the held producer's last receipt; a held marker for a unit never reconciled | M3 |
| [`drift`](drift/scenario.yaml) | Drift: `verify` finds drift, `reconcile` repairs the idempotent unit, the other awaits approval, and `driftRepair: manual` waits for `--reapply` | M3 |
| [`stale-promotion-evidence`](stale-promotion-evidence/scenario.yaml) | Promotion with stale source evidence | M6 |

These files are the contract's acceptance tests.
[`scenario_harness_test.rs`](../../scenario_harness_test.rs) validates every
scenario against the format, applies its Git steps in a test world, and runs
the scenarios whose milestone it has reached in tier 1 through
[`scenario/runner.rs`](../../scenario/runner.rs): each `nyl` step runs the
real command line in-process in a fresh clone, with a fake clock, sequential
identifiers, and the fake kinds. A `nyl` step with `pause` runs on a thread
of its own and blocks at its pause point until its `resume` step, so later
steps run while it waits; a paused step that never reaches its point, or is
never resumed, fails the scenario. A reached scenario runs, or is listed there
as pending with what it waits for.
[`walkthrough_resources_test.rs`](../../walkthrough_resources_test.rs)
parses and validates every project resource whose kind has a Rust type, and
lists the kinds later milestones add. A `file` step changes a file beneath
the scenario's temporary directory, standing in for a change made outside
Nyl; Command units reach that directory through `env.passthrough:
[SCENARIO_TMP]`, which the runner sets. A change to a rule that a
walkthrough uses changes the walkthrough and its scenario in the same change.

