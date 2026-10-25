# Crash-injection scenarios

Each directory kills one `nyl reconcile` at one of the shell's killable
fault points and checks how the next run takes over, as the
[runs and leases contract](../../../../design/orchestration-core.md#runs-and-leases)
describes. They run in tier 1 only and share the walkthroughs'
[`_project`](../walkthroughs/_project): `network` and `web-image` run in wave 1
(one batch), `database` in wave 2, and `kubernetes` in wave 3, and every fake
kind's recovery policy is `converge`.

Every scenario follows the same steps: `state init`; a `fault` step; a
reconcile that is killed there; a reconcile one minute before the dead run's
deadline that exits 2 with `NYL-LEASE-HELD` and writes nothing; and, once the
clock passes the deadline, a reconcile that takes over and exits 0 with the
`imported`, `recovered`, and `executed` sets the point leaves behind.

| Scenario | Killed | Deadline | Next run |
| --- | --- | --- | --- |
| [`after-effect`](after-effect/scenario.yaml) | After `network`'s effect | Wave 1: 60m + 10m | Recovers `network` and `web-image` |
| [`before-checkpoint`](before-checkpoint/scenario.yaml) | Before `web-image`'s checkpoint | Wave 1: 60m + 10m | Imports `network`, recovers `web-image` |
| [`before-lease-update`](before-lease-update/scenario.yaml) | Before the lease update that starts `database`'s phase | The resolution before it: 30m + 10m | Imports wave 1, executes `database` |
| [`before-push`](before-push/scenario.yaml) | Before the final transition commit | The last resolution: 30m + 10m | Imports every unit, executes nothing |

A run resolves again before each wave, and that resolution phase renews the
lease for the resolution timeout, so a kill after a resolution leaves its
30m + 10m deadline rather than the previous wave's.
