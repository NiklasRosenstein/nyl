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
//! - A broken lease (`nyl lease break`) whose run no operation has imported
//!   yet ([`unimported_break`]) is taken over at once, but only by an
//!   operation that imports that run: `reconcile`, `teardown`, and `recover`
//!   ([`BrokenLease::TakeOver`]). Every other state-writing operation gets
//!   [`LeaseDecision::Broken`] (`NYL-LEASE-BROKEN`, naming `nyl recover -e
//!   <env>`), so the dead run's effects are recorded before anything else
//!   writes state (item 1). The importer records the break once, in its
//!   transition commit (item 8).
//! - An expired, unbroken lease is taken over by every operation.
//! - Otherwise the run takes the lease by compare-and-swap against the
//!   version it read, and the expired runs are left for import.

use nyl_core::orchestration::ids::RunId;
use nyl_core::orchestration::ports::{Coordination, RunState};
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

/// Whether an operation imports the run of a broken lease and so may take
/// such a lease over: `reconcile`, `teardown`, and `recover` do; every other
/// state-writing operation refuses with `NYL-LEASE-BROKEN`. Contract:
/// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), items 1 and 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokenLease {
    /// The operation imports the broken run: it takes the lease over.
    TakeOver,
    /// The operation does not import: a broken lease whose run is not
    /// imported yet stops it like a held lease.
    Refuse,
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
    /// The lease is broken and its run not imported, and the caller does not
    /// import: `NYL-LEASE-BROKEN`, naming `nyl recover -e <env>`.
    Broken(Lease),
}

/// Whether a lease is still held at `now`: unbroken and before its deadline.
pub(crate) fn lease_live(lease: &Lease, now: Timestamp) -> bool {
    lease.broken.is_none() && lease.deadline > now
}

/// Whether an unfinished run ref is still live at `now`: before its deadline
/// and not the run of a broken lease, which declares its holder gone, its
/// run ref included.
pub(crate) fn run_live(coordination: &Coordination, state: &RunState, now: Timestamp) -> bool {
    let broken = coordination
        .lease
        .as_ref()
        .filter(|held| held.lease.broken.is_some())
        .map(|held| &held.lease.run);
    !state.run.finished && state.run.deadline > now && Some(&state.run.run) != broken
}

/// The broken lease whose run no operation has imported yet, if any: the
/// lease ref is still the broken one (an importer replaces it, and its
/// commit records the break once). The one predicate behind both
/// [`decide`] (a non-importer gets [`LeaseDecision::Broken`]) and `nyl
/// recover -e` (which imports whenever this is `Some`, recording the break
/// with `TransitionChanges.commit_unchanged` even when nothing else is left
/// to import). Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
/// items 1 and 8.
pub fn unimported_break(coordination: &Coordination) -> Option<&Lease> {
    coordination
        .lease
        .as_ref()
        .map(|held| &held.lease)
        .filter(|lease| lease.broken.is_some())
}

/// Who holds the environment at `now`: a live lease first, then live,
/// unfinished runs it does not name.
pub(crate) fn holders(coordination: &Coordination, now: Timestamp) -> Vec<Holder> {
    let mut holders = Vec::new();
    let mut lease_run: Option<&RunId> = None;
    if let Some(held) = coordination.lease.as_ref().filter(|held| lease_live(&held.lease, now)) {
        lease_run = Some(&held.lease.run);
        holders.push(Holder::Lease(held.lease.clone()));
    }
    for state in &coordination.runs {
        if run_live(coordination, state, now) && Some(&state.run.run) != lease_run {
            holders.push(Holder::Run(state.run.clone()));
        }
    }
    holders
}

/// Decide from one read of the coordination refs at `now`, for an
/// operation that does or does not import a broken lease's run: a live
/// holder holds first; then a broken lease whose run is not imported is
/// [`LeaseDecision::Broken`] for a non-importer and taken over by an
/// importer, which reports the break; anything else is taken. Contract:
/// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), items 1 and 8.
pub fn decide(coordination: &Coordination, now: Timestamp, broken: BrokenLease) -> LeaseDecision {
    if let Some(holder) = holders(coordination, now).into_iter().next() {
        return LeaseDecision::Held(holder);
    }
    if let (Some(lease), BrokenLease::Refuse) = (unimported_break(coordination), broken) {
        return LeaseDecision::Broken(lease.clone());
    }
    LeaseDecision::Take {
        expected: coordination.lease.as_ref().map(|held| held.version.clone()),
        broken: unimported_break(coordination).and_then(|lease| lease.broken.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyl_core::orchestration::ports::HeldLease;

    use crate::rules::fixtures::{lease_record, run_record, run_state, RUN_1 as RUN_A, RUN_2 as RUN_B};

    fn time(minutes: i64) -> Timestamp {
        chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc()
            + chrono::Duration::minutes(minutes)
    }

    fn lease(run: &str, deadline: i64) -> HeldLease {
        HeldLease {
            lease: lease_record(run, time(deadline)),
            version: "v1".to_owned(),
        }
    }

    fn run(id: &str, deadline: i64, finished: bool) -> RunState {
        run_state(Run {
            finished,
            ..run_record(id, time(deadline))
        })
    }

    #[test]
    fn test_a_lease_within_its_deadline_is_held_and_an_expired_one_is_taken_over() {
        let coordination = Coordination {
            lease: Some(lease(RUN_A, 70)),
            runs: Vec::new(),
            evidence: Vec::new(),
        };
        assert!(matches!(
            decide(&coordination, time(69), BrokenLease::TakeOver),
            LeaseDecision::Held(Holder::Lease(_))
        ));
        assert_eq!(
            decide(&coordination, time(70), BrokenLease::TakeOver),
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
            decide(&coordination, time(10), BrokenLease::TakeOver),
            LeaseDecision::Held(Holder::Run(_))
        ));
        // A finished run ref no longer counts, whatever its deadline.
        let coordination = Coordination {
            lease: None,
            runs: vec![run(RUN_A, 70, true)],
            evidence: Vec::new(),
        };
        assert_eq!(
            decide(&coordination, time(10), BrokenLease::TakeOver),
            LeaseDecision::Take {
                expected: None,
                broken: None
            }
        );
    }

    fn broken(run: &str, deadline: i64) -> HeldLease {
        let mut held = lease(run, deadline);
        held.lease.broken = Some(Request {
            by: "alice".to_owned(),
            at: time(5),
            reason: Some("runner lost".to_owned()),
        });
        held
    }

    /// Catches an importer (`reconcile`, `teardown`, `recover`) refused by a
    /// break it exists to resolve, or one that loses the break it must record.
    #[test]
    fn test_an_importer_takes_a_broken_lease_over_at_once_and_reports_it() {
        let coordination = Coordination {
            lease: Some(broken(RUN_A, 70)),
            runs: vec![run(RUN_A, 70, false)],
            evidence: Vec::new(),
        };
        let LeaseDecision::Take { expected, broken } = decide(&coordination, time(10), BrokenLease::TakeOver) else {
            panic!("an importer takes a broken lease over");
        };
        assert_eq!(expected.as_deref(), Some("v1"));
        assert_eq!(broken.unwrap().by, "alice");
    }

    /// Catches a state operation (or `hold`, `verify`) writing state before
    /// the broken run's effects are imported.
    #[test]
    fn test_a_non_importer_is_refused_by_a_broken_lease() {
        let coordination = Coordination {
            lease: Some(broken(RUN_A, 70)),
            runs: vec![run(RUN_A, 70, false)],
            evidence: Vec::new(),
        };
        let LeaseDecision::Broken(lease) = decide(&coordination, time(10), BrokenLease::Refuse) else {
            panic!("a non-importer is refused");
        };
        assert_eq!(lease.run.as_str(), RUN_A);
        // Also after the broken lease's deadline: a break is resolved only by an import.
        assert!(matches!(
            decide(&coordination, time(200), BrokenLease::Refuse),
            LeaseDecision::Broken(_)
        ));
    }

    /// Catches a break overriding another run that is still live: its run
    /// ref holds the environment for importers and non-importers alike.
    #[test]
    fn test_another_live_run_ref_still_holds_a_broken_environment() {
        let coordination = Coordination {
            lease: Some(broken(RUN_A, 70)),
            runs: vec![run(RUN_A, 70, false), run(RUN_B, 70, false)],
            evidence: Vec::new(),
        };
        for broken in [BrokenLease::TakeOver, BrokenLease::Refuse] {
            assert!(matches!(
                decide(&coordination, time(10), broken),
                LeaseDecision::Held(Holder::Run(ref run)) if run.run.as_str() == RUN_B
            ));
        }
    }

    /// Catches a non-importer refused by an ordinary expired lease, which
    /// any operation takes over.
    #[test]
    fn test_an_expired_unbroken_lease_is_taken_by_a_non_importer() {
        let coordination = Coordination {
            lease: Some(lease(RUN_A, 70)),
            runs: vec![run(RUN_A, 70, false)],
            evidence: Vec::new(),
        };
        assert_eq!(
            decide(&coordination, time(70), BrokenLease::Refuse),
            LeaseDecision::Take {
                expected: Some("v1".to_owned()),
                broken: None
            }
        );
    }

    proptest::proptest! {
        /// Catches the lease rule and `nyl recover -e` disagreeing about
        /// whether a break awaits its import: with no live holder, a
        /// non-importer is refused exactly when `unimported_break` is set,
        /// and an importer is never refused by a break.
        #[test]
        fn prop_a_non_importer_is_refused_exactly_when_a_break_awaits_import(
            has_lease in proptest::bool::ANY,
            is_broken in proptest::bool::ANY,
            lease_deadline in 0i64..100,
            runs in proptest::collection::vec((0i64..100, proptest::bool::ANY, proptest::bool::ANY), 0..3),
            now in 0i64..100,
        ) {
            let lease = has_lease.then(|| if is_broken { broken(RUN_A, lease_deadline) } else { lease(RUN_A, lease_deadline) });
            let runs = runs
                .into_iter()
                .map(|(deadline, finished, other)| run(if other { RUN_B } else { RUN_A }, deadline, finished))
                .collect();
            let coordination = Coordination { lease, runs, evidence: Vec::new() };
            let held = !holders(&coordination, time(now)).is_empty();
            let refused = decide(&coordination, time(now), BrokenLease::Refuse);
            proptest::prop_assert_eq!(
                matches!(refused, LeaseDecision::Broken(_)),
                !held && unimported_break(&coordination).is_some()
            );
            let importer = decide(&coordination, time(now), BrokenLease::TakeOver);
            proptest::prop_assert!(!matches!(importer, LeaseDecision::Broken(_)));
            if let LeaseDecision::Take { broken, .. } = importer {
                proptest::prop_assert_eq!(broken.is_some(), unimported_break(&coordination).is_some());
            }
        }
    }
}
