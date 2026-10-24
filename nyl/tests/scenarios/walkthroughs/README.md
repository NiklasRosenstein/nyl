# Contract walkthroughs

Each directory is one walkthrough from the
[orchestration core contract](../../../../design/orchestration-core.md#walkthroughs),
written in the scenario format that the
[reference scenarios](../../../../design/reference-scenarios.md#scenario-files)
define. `_project/` is the project they share unless a scenario brings its own.

| Scenario | Walkthrough | Runs from |
| --- | --- | --- |
| [`dependency-wave`](dependency-wave/scenario.yaml) | Successful dependency wave | M3 |
| [`unit-jobs`](unit-jobs/scenario.yaml) | Separate unit-scoped CI invocations | M3 |
| [`ci-admission`](ci-admission/scenario.yaml) | Superseded worker retries, idempotent dispatch, and authorized rollback | M8 |
| [`ci-lifecycle`](ci-lifecycle/scenario.yaml) | Leaving units in planning and a separate lifecycle job | M8 |
| [`unavailable-upstream-output`](unavailable-upstream-output/scenario.yaml) | Unavailable upstream output | M3 |
| [`effects-without-receipt`](effects-without-receipt/scenario.yaml) | Effects without a receipt | M3 |
| [`competing-runners`](competing-runners/scenario.yaml) | Competing runners | M3 |
| [`deletion`](deletion/scenario.yaml) | Deletion | M3 |
| [`stale-promotion-evidence`](stale-promotion-evidence/scenario.yaml) | Promotion with stale source evidence | M6 |

These files are the contract's acceptance tests. The M3 scenario harness runs
them; until it exists, nothing executes them.
[`walkthrough_resources_test.rs`](../../walkthrough_resources_test.rs) already
parses and validates every project resource whose kind exists today, and lists
the kinds later milestones add. A change to a rule that a
walkthrough uses changes the walkthrough and its scenario in the same change.

