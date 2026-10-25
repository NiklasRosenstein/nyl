//! Whether a run may take an environment's lease.
//!
//! Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases).
//!
//! - A lease whose deadline has not passed is held. Deadlines already
//!   include the grace period that absorbs clock skew, so nothing is added
//!   here.
//! - A run ref whose `Run` record is not finished and whose deadline has not
//!   passed counts as a held lease too, so a deleted lease ref alone never
//!   lets a second run execute units the first is still executing.
//! - A broken lease (`nyl lease break`) is taken over at once, and the break
//!   is recorded in the taking run's transition commit.
//! - Otherwise the run takes the lease by compare-and-swap against the
//!   version it read, and the expired runs are left for import.

use nyl_core::orchestration::ports::Coordination;
use nyl_core::orchestration::state::{Lease, Request, Run};
use nyl_core::orchestration::time::Timestamp;

/// Who holds an environment: its lease, or a run ref still within its deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
    Lease(Lease),
    Run(Run),
}

impl Holder {
    /// A one-line description naming the run, runner, operation, and deadline.
    pub fn describe(&self) -> String {
        let (run, runner, operation, deadline) = match self {
            Self::Lease(lease) => (&lease.run, &lease.runner, lease.operation, lease.deadline),
            Self::Run(run) => (&run.run, &run.runner, run.operation, run.deadline),
        };
        format!(
            "run {run} ({}) by {runner}, until {}",
            operation.as_str(),
            deadline.to_rfc3339()
        )
    }
}

/// What a run may do about the lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseDecision {
    /// Take the lease by compare-and-swap against `expected` (`None`: no
    /// lease ref exists); `broken` carries a recorded break to report.
    Take {
        expected: Option<String>,
        broken: Option<Request>,
    },
    /// Another run holds the environment.
    Held(Holder),
}

/// Decide from one read of the coordination refs at `now`.
pub fn decide(coordination: &Coordination, now: Timestamp) -> LeaseDecision {
    if let Some(held) = &coordination.lease {
        if held.lease.broken.is_none() && held.lease.deadline > now {
            return LeaseDecision::Held(Holder::Lease(held.lease.clone()));
        }
    }
    // A broken lease declares its holder gone, its run ref included.
    let broken_run = coordination
        .lease
        .as_ref()
        .filter(|held| held.lease.broken.is_some())
        .map(|held| &held.lease.run);
    if let Some(state) = coordination
        .runs
        .iter()
        .find(|state| !state.run.finished && state.run.deadline > now && Some(&state.run.run) != broken_run)
    {
        return LeaseDecision::Held(Holder::Run(state.run.clone()));
    }
    LeaseDecision::Take {
        expected: coordination.lease.as_ref().map(|held| held.version.clone()),
        broken: coordination.lease.as_ref().and_then(|held| held.lease.broken.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::ids::RunId;
    use nyl_core::orchestration::ports::{HeldLease, RunState};
    use nyl_core::orchestration::state::{LeaseKind, Operation, RunKind, StateV1};

    const RUN_A: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
    const RUN_B: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91";

    fn time(minutes: i64) -> Timestamp {
        chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc()
            + chrono::Duration::minutes(minutes)
    }

    fn lease(run: &str, deadline: i64) -> HeldLease {
        HeldLease {
            lease: Lease {
                api_version: StateV1,
                kind: LeaseKind,
                run: RunId::new(run).unwrap(),
                runner: "ci".to_owned(),
                operation: Operation::Reconcile,
                units: Vec::new(),
                deadline: time(deadline),
                taken_at: time(0),
                broken: None,
            },
            version: "v1".to_owned(),
        }
    }

    fn run(id: &str, deadline: i64, finished: bool) -> RunState {
        RunState {
            run: Run {
                api_version: StateV1,
                kind: RunKind,
                run: RunId::new(id).unwrap(),
                runner: "ci".to_owned(),
                operation: Operation::Reconcile,
                source_commit: None,
                read_desired: None,
                read_observed: None,
                deadline: time(deadline),
                units: Vec::new(),
                finished,
            },
            checkpoints: Vec::new(),
            created: Vec::new(),
            foreign: false,
            version: "r1".to_owned(),
        }
    }

    #[test]
    fn test_a_lease_within_its_deadline_is_held_and_an_expired_one_is_taken_over() {
        let coordination = Coordination {
            lease: Some(lease(RUN_A, 70)),
            runs: Vec::new(),
            evidence: Vec::new(),
        };
        assert!(matches!(
            decide(&coordination, time(69)),
            LeaseDecision::Held(Holder::Lease(_))
        ));
        assert_eq!(
            decide(&coordination, time(70)),
            LeaseDecision::Take {
                expected: Some("v1".to_owned()),
                broken: None
            }
        );
    }

    #[test]
    fn test_a_live_run_ref_holds_the_environment_without_a_lease_ref() {
        let coordination = Coordination {
            lease: None,
            runs: vec![run(RUN_A, 70, false)],
            evidence: Vec::new(),
        };
        assert!(matches!(
            decide(&coordination, time(10)),
            LeaseDecision::Held(Holder::Run(_))
        ));
        // A finished run ref no longer counts, whatever its deadline.
        let coordination = Coordination {
            lease: None,
            runs: vec![run(RUN_A, 70, true)],
            evidence: Vec::new(),
        };
        assert_eq!(
            decide(&coordination, time(10)),
            LeaseDecision::Take {
                expected: None,
                broken: None
            }
        );
    }

    #[test]
    fn test_a_broken_lease_is_taken_over_at_once_and_reported() {
        let mut held = lease(RUN_A, 70);
        held.lease.broken = Some(Request {
            by: "alice".to_owned(),
            at: time(5),
            reason: Some("runner lost".to_owned()),
        });
        let coordination = Coordination {
            lease: Some(held),
            runs: vec![run(RUN_A, 70, false)],
            evidence: Vec::new(),
        };
        let LeaseDecision::Take { broken, .. } = decide(&coordination, time(10)) else {
            panic!("a broken lease is taken over");
        };
        assert_eq!(broken.unwrap().by, "alice");
        // Another run's live run ref still holds the environment.
        let mut coordination = coordination;
        coordination.runs.push(run(RUN_B, 70, false));
        assert!(matches!(
            decide(&coordination, time(10)),
            LeaseDecision::Held(Holder::Run(_))
        ));
    }
}
