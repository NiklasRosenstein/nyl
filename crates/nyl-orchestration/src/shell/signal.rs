//! Signals to a running run: `--confirm-removed` and `nyl hold`.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! the signal path, and [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit", as amended by the part 5 decisions (Q28):
//!
//! - `--confirm-removed`: with no lease held, confirm directly; when the
//!   lease holder waits on that unit's teardown, push a `confirm-removed`
//!   `Signal` to its signal ref naming the operator, reason, time, run ID,
//!   and phase 1 commit, report where it went, and exit 0; when the holder
//!   does anything else, exit 2 as for a held lease. A confirmation for a
//!   unit the run is not waiting on is rejected when it is sent
//!   (`NYL-CONFIRMATION-REJECTED`).
//! - `nyl hold`: with no lease held, hold directly; when a run holds the
//!   lease, push a `hold` signal to its signal ref, report that it was
//!   delivered (`NYL-HOLD-DELIVERED`), and exit 0. The run checks its
//!   signals before starting each unit ([`received`]), starts no held unit,
//!   writes the hold in its own transition commit, and reports the unit as
//!   held.
//! - Either way, after pushing, check that the lease still belongs to that
//!   run; if the run finished meanwhile, delete the signal and act directly
//!   under the invocation's own lease ([`Delivery::Direct`]).

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::Coordination;
use nyl_core::orchestration::state::{
    ApprovalIdentity, Signal, SignalAction, SignalKind, StateLocation, StateV1, TeardownWaiting,
};
use nyl_core::orchestration::time::Timestamp;

use super::{store_error, Context};
use crate::rules::lease::{holders, Holder};

/// Where a signal goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// No run holds the lease (or it finished before reading the signal):
    /// the invocation acts directly, under its own lease.
    Direct,
    /// Delivered to the running run, which acts on it.
    Delivered { run: RunId, runner: String },
}

/// Decide where `action` for `unit` goes, sending the signal when a run
/// holds the lease (for `confirm-removed`, a run waiting on that unit).
///
/// A broken lease is no live holder: the caller refuses it (or, for an
/// importer such as `nyl teardown`, takes it over) as for any operation.
pub fn deliver(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    unit: &UnitName,
    action: SignalAction,
    reason: Option<&str>,
) -> Result<Delivery, Finding> {
    let now = context.clock.now();
    let coordination = context
        .store
        .coordination(location, environment)
        .map_err(|error| store_error(environment, &error))?;
    let Some(holder) = holders(&coordination, now).into_iter().next() else {
        return match action {
            SignalAction::Hold => Ok(Delivery::Direct),
            SignalAction::ConfirmRemoved => {
                // With no run holding the lease, the wait a direct
                // confirmation ends is the one stored in observed state.
                let snapshot = context
                    .store
                    .snapshot(location)
                    .map_err(|error| store_error(environment, &error))?;
                if snapshot
                    .observed
                    .get(unit)
                    .is_some_and(|observed| observed.waiting.is_some())
                {
                    Ok(Delivery::Direct)
                } else {
                    Err(rejected(environment, unit))
                }
            }
        };
    };
    let (run, runner) = match &holder {
        Holder::Lease(lease) => (lease.run.clone(), lease.runner.clone()),
        Holder::Run(record) => (record.run.clone(), record.runner.clone()),
    };
    let phase1_commit = match action {
        SignalAction::Hold => None,
        SignalAction::ConfirmRemoved => match waiting_on(&coordination, &holder, unit) {
            Some(waiting) => waiting.phase1_commit.clone(),
            None => {
                return Err(Finding::new(
                    Code::LeaseHeld,
                    format!(
                        "{environment} is held by {}, which is not waiting on the teardown of {unit}",
                        holder.describe()
                    ),
                    "wait for that run to finish, then confirm again; a confirmation reaches a running run only while it waits on the unit's teardown",
                ))
            }
        },
    };
    let signal = Signal {
        api_version: StateV1,
        kind: SignalKind,
        run: run.clone(),
        action,
        unit: unit.clone(),
        phase1_commit,
        by: context.requested_by.clone(),
        identity: ApprovalIdentity::Asserted,
        reason: reason.map(ToOwned::to_owned),
        at: now,
        from: context.runner.clone(),
    };
    context
        .store
        .send_signal(location, environment, &signal)
        .map_err(|error| store_error(environment, &error))?;

    // The run may have finished between the read and the push, without
    // reading the signal: then the invocation acts directly.
    let after = context
        .store
        .coordination(location, environment)
        .map_err(|error| store_error(environment, &error))?;
    if holds(&after, context.clock.now(), &run) {
        return Ok(Delivery::Delivered { run, runner });
    }
    // A signal left behind is bound to a finished run and never acts, so a
    // failure to delete it does not stop the direct action.
    let _ = context.store.delete_signals(location, environment, &run);
    Ok(Delivery::Direct)
}

/// The teardown wait `holder` sits in for `unit`: its lease (or run record)
/// lists the unit, and its run ref's latest checkpoint of the unit records
/// a teardown wait. `None` when the holder does something else.
fn waiting_on<'a>(coordination: &'a Coordination, holder: &Holder, unit: &UnitName) -> Option<&'a TeardownWaiting> {
    let (run, units) = match holder {
        Holder::Lease(lease) => (&lease.run, &lease.units),
        Holder::Run(record) => (&record.run, &record.units),
    };
    if !units.contains(unit) {
        return None;
    }
    let state = coordination.runs.iter().find(|state| &state.run.run == run)?;
    let checkpoint = state
        .checkpoints
        .iter()
        .rev()
        .find(|checkpoint| &checkpoint.observed.unit.name == unit)?;
    checkpoint.observed.waiting.as_ref()
}

/// Whether `run` still holds the environment at `now`.
fn holds(coordination: &Coordination, now: Timestamp, run: &RunId) -> bool {
    holders(coordination, now).iter().any(|holder| match holder {
        Holder::Lease(lease) => &lease.run == run,
        Holder::Run(record) => &record.run == run,
    })
}

/// `NYL-CONFIRMATION-REJECTED`: no teardown of `unit` waits.
fn rejected(environment: &EnvironmentName, unit: &UnitName) -> Finding {
    Finding::new(
        Code::ConfirmationRejected,
        format!("no teardown of {environment}/{unit} waits after its first phase, so there is no removal to confirm"),
        format!(
            "confirm only a publication whose teardown waits after phase 1 (`nyl status -e {environment}` shows it tearing-down); run `nyl teardown -e {environment} --unit {unit}` to start its teardown"
        ),
    )
}

/// The signals sent to `run` that it has not acted on yet, oldest first.
/// The run path reads them before starting each unit (holds) and while a
/// teardown waits (confirmations); a store failure reads as none.
#[allow(dead_code)] // Skeleton: package `lifecycle` calls it from the run path.
pub(crate) fn received(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    run: &RunId,
) -> Vec<Signal> {
    // An unreadable signal ref is read again at the next poll; it never
    // fails the run that waits.
    let mut signals = context.store.signals(location, environment, run).unwrap_or_default();
    signals.sort_by_key(|signal| signal.at);
    signals
}

/// Test doubles for reading signals: a state store holding only signal
/// refs, shared with the waiting loop's tests.
#[cfg(test)]
pub(crate) mod doubles {
    use std::sync::Mutex;

    use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
    use nyl_core::orchestration::ports::{
        Checkpoint, Committed, Coordination, EnvironmentSnapshot, StateError, StateResult, StateStore, StateTips,
        Transition,
    };
    use nyl_core::orchestration::state::{
        ApprovalIdentity, Lease, Run, Signal, SignalAction, SignalKind, StateLocation, StateV1,
    };
    use nyl_core::orchestration::time::Timestamp;

    use crate::fault::NoFaults;
    use crate::shell::{Context, Settings};
    use crate::testing::SequentialIds;

    pub(crate) fn location() -> StateLocation {
        StateLocation {
            repository: "https://example.com/state.git".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: String::new(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        }
    }

    pub(crate) fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    pub(crate) fn time(seconds: i64) -> Timestamp {
        chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc()
            + chrono::Duration::seconds(seconds)
    }

    pub(crate) fn signal(action: SignalAction, run: &RunId, unit: &str, phase1: Option<&CommitId>, at: i64) -> Signal {
        Signal {
            api_version: StateV1,
            kind: SignalKind,
            run: run.clone(),
            action,
            unit: UnitName::new(unit).unwrap(),
            phase1_commit: phase1.cloned(),
            by: "alice".to_owned(),
            identity: ApprovalIdentity::Asserted,
            reason: None,
            at: time(at),
            from: "https://ci.example.com/runs/1".to_owned(),
        }
    }

    /// Signal refs only; every other call is a test bug.
    #[derive(Default)]
    pub(crate) struct SignalStore {
        pub signals: Mutex<Vec<Signal>>,
        pub unavailable: bool,
    }

    impl SignalStore {
        pub(crate) fn push(&self, signal: Signal) {
            self.signals.lock().unwrap().push(signal);
        }
    }

    impl StateStore for SignalStore {
        fn snapshot(&self, _: &StateLocation) -> StateResult<EnvironmentSnapshot> {
            unimplemented!("signal reads only")
        }
        fn coordination(&self, _: &StateLocation, _: &EnvironmentName) -> StateResult<Coordination> {
            unimplemented!("signal reads only")
        }
        fn take_lease(
            &self,
            _: &StateLocation,
            _: &EnvironmentName,
            _: &Lease,
            _: Option<&str>,
        ) -> StateResult<String> {
            unimplemented!("signal reads only")
        }
        fn update_lease(&self, _: &StateLocation, _: &EnvironmentName, _: &Lease, _: &str) -> StateResult<String> {
            unimplemented!("signal reads only")
        }
        fn release_lease(&self, _: &StateLocation, _: &EnvironmentName, _: &str) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
        fn delete_lease(&self, _: &StateLocation, _: &EnvironmentName) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
        fn write_run(
            &self,
            _: &StateLocation,
            _: &EnvironmentName,
            _: &Run,
            _: Option<&Checkpoint>,
        ) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
        fn delete_run(&self, _: &StateLocation, _: &EnvironmentName, _: &RunId, _: Option<&str>) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
        fn retire_run(&self, _: &StateLocation, _: &EnvironmentName, _: &RunId, _: Option<&str>) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
        fn delete_evidence(&self, _: &StateLocation, _: &EnvironmentName, _: &RunId) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
        fn signals(&self, _: &StateLocation, _: &EnvironmentName, run: &RunId) -> StateResult<Vec<Signal>> {
            if self.unavailable {
                return Err(StateError::Unavailable("signal ref unreadable".to_owned()));
            }
            Ok(self
                .signals
                .lock()
                .unwrap()
                .iter()
                .filter(|signal| &signal.run == run)
                .cloned()
                .collect())
        }
        fn send_signal(&self, _: &StateLocation, _: &EnvironmentName, signal: &Signal) -> StateResult<()> {
            self.push(signal.clone());
            Ok(())
        }
        fn commit(
            &self,
            _: &StateLocation,
            _: &EnvironmentName,
            _: &StateTips,
            _: &Transition,
            _: &str,
        ) -> StateResult<Committed> {
            unimplemented!("signal reads only")
        }
        fn import_history(&self, _: &StateLocation, _: &StateLocation) -> StateResult<()> {
            unimplemented!("signal reads only")
        }
    }

    pub(crate) fn context<'a>(
        store: &'a dyn StateStore,
        clock: &'a dyn nyl_core::orchestration::ports::Clock,
    ) -> Context<'a> {
        Context {
            store,
            clock,
            // Signal reads and waits generate no identifiers; each context
            // gets its own generator all the same.
            ids: Box::leak(Box::new(SequentialIds::new())),
            faults: &NoFaults,
            runner: "bob@workstation".to_owned(),
            requested_by: "Bob <bob@example.invalid>".to_owned(),
            version: "test".to_owned(),
            settings: Settings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::doubles::{context, dev, location, signal, time, SignalStore};
    use super::*;
    use crate::testing::FakeClock;

    fn run(id: &str) -> RunId {
        RunId::new(id).unwrap()
    }

    /// Catches a run acting on a later hold before an earlier confirmation,
    /// or on another run's signals.
    #[test]
    fn test_received_returns_the_runs_signals_oldest_first() {
        let store = SignalStore::default();
        let ours = run("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90");
        let theirs = run("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91");
        store.push(signal(SignalAction::Hold, &ours, "web", None, 30));
        store.push(signal(SignalAction::ConfirmRemoved, &theirs, "web", None, 5));
        store.push(signal(SignalAction::ConfirmRemoved, &ours, "kubernetes", None, 10));
        let clock = FakeClock::new(time(60));
        let context = context(&store, &clock);
        let signals = received(&context, &location(), &dev(), &ours);
        assert_eq!(
            signals
                .iter()
                .map(|signal| (signal.action, signal.at))
                .collect::<Vec<_>>(),
            vec![(SignalAction::ConfirmRemoved, time(10)), (SignalAction::Hold, time(30))]
        );
    }

    /// Catches an unreadable signal ref failing the run that waits.
    #[test]
    fn test_received_reads_an_unavailable_store_as_no_signals() {
        let store = SignalStore {
            unavailable: true,
            ..SignalStore::default()
        };
        let clock = FakeClock::new(time(0));
        let context = context(&store, &clock);
        assert!(received(
            &context,
            &location(),
            &dev(),
            &run("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90")
        )
        .is_empty());
    }
}
