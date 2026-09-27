# Contract walkthroughs

Each directory is one walkthrough from the
[orchestration core contract](../../../../design/orchestration-core.md#walkthroughs),
written in the scenario format that the
[reference scenarios](../../../../design/reference-scenarios.md#scenario-files)
define. `_project/` is the project they share unless a scenario brings its own.

| Scenario | Walkthrough | Runs from |
| --- | --- | --- |
| [`dependency-wave`](dependency-wave/scenario.yaml) | Successful dependency wave | M3 |
| [`unavailable-upstream-output`](unavailable-upstream-output/scenario.yaml) | Unavailable upstream output | M3 |
| [`effects-without-receipt`](effects-without-receipt/scenario.yaml) | Effects without a receipt | M3 |
| [`competing-runners`](competing-runners/scenario.yaml) | Competing runners | M3 |
| [`deletion`](deletion/scenario.yaml) | Deletion | M3 |
| [`stale-promotion-evidence`](stale-promotion-evidence/scenario.yaml) | Promotion with stale source evidence | M6 |

These files are the contract's acceptance tests. The M3 scenario harness runs
them; until it exists, nothing executes them. A change to a rule that a
walkthrough uses changes the walkthrough and its scenario in the same change.
