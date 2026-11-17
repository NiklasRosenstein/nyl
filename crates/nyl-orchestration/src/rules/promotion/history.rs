//! Recorded states: the transition commit pairs of an environment, found
//! from its desired and observed history by `Nyl-Run-Id`.
//!
//! Contract: [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source),
//! "The unit of promotion is one recorded state", and the orchestration core
//! contract's [Transition commits](../../../../../design/orchestration-core.md#transition-commits).
//!
//! [`recorded_states`] is pure over the two logs
//! ([`StateStore::log`](nyl_core::orchestration::ports::StateStore::log), newest
//! first). The shell reads the documents at each state's commits and the
//! attestations recorded for it into a [`RecordedState`], which the rest of
//! the promotion rules take.
//!
//! - A state is a desired-ref commit of a `reconcile` or `recover` run (the
//!   operations that record receipts) of this environment, paired with the
//!   observed-ref commit of the same `Nyl-Run-Id`. One ref shared by both
//!   sides pairs a commit with itself.
//! - A state's desired commit is the state commit ([`DesiredEntry`]), which may
//!   be older than the run's own commit.
//! - Commits of other operations (`attest`, `promote`, `detect-drift`, ...)
//!   and of other environments sharing the ref are not states. A `Nyl-Local`
//!   commit is not shared evidence and is skipped.
//! - Newest first; a run appears once.

use std::collections::BTreeMap;

use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, LogEntry};
use nyl_core::orchestration::state::{Attestation, Operation};

use crate::transition::trailer;

/// The operations whose transition commits record receipts, and so a state.
const STATE_OPERATIONS: [Operation; 2] = [Operation::Reconcile, Operation::Recover];

/// The commits of one recorded state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRef {
    /// The `Nyl-Run-Id` of the run.
    pub run: RunId,
    /// The state's commit on the desired ref.
    pub desired_commit: CommitId,
    /// The run's commit on the observed ref, when it wrote one.
    pub observed_commit: Option<CommitId>,
}

/// One commit of the desired ref's first-parent history with what only the
/// store can tell: whether it changed this environment's `desired/` documents
/// (beneath its location's path), as [`StateStore::state_commit`] asks.
///
/// [`StateStore::state_commit`]: nyl_core::orchestration::ports::StateStore::state_commit
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesiredEntry {
    pub entry: LogEntry,
    pub changed_desired: bool,
}

/// The states `environment` recorded, newest first.
///
/// A state's `desired_commit` is its state commit, defined as
/// `StateStore::state_commit` defines it: the newest commit at or before the
/// run's own commit, along first parents, that changed the environment's
/// desired documents, because that is the commit environment attestations
/// name. A run whose own commit changed none (on one ref shared by both
/// sides, a run that changed only observed documents) takes the older commit
/// that did; a run with none at all records no state.
pub fn recorded_states(
    environment: &EnvironmentName,
    desired: &[DesiredEntry],
    observed: &[LogEntry],
) -> Vec<StateRef> {
    let mut states: Vec<StateRef> = Vec::new();
    for (index, entry) in desired.iter().enumerate() {
        let Some(run) = state_run(environment, &entry.entry.message) else {
            continue;
        };
        if states.iter().any(|state| state.run == run) {
            continue;
        }
        let Some(state_commit) = desired[index..].iter().find(|older| older.changed_desired) else {
            continue;
        };
        let observed_commit = observed
            .iter()
            .find(|candidate| state_run(environment, &candidate.message).as_ref() == Some(&run))
            .map(|candidate| candidate.commit.clone());
        states.push(StateRef {
            run,
            desired_commit: state_commit.entry.commit.clone(),
            observed_commit,
        });
    }
    states
}

/// The run of a commit message that records a state of `environment`.
fn state_run(environment: &EnvironmentName, message: &str) -> Option<RunId> {
    let operation = trailer(message, "Nyl-Operation")?;
    if !STATE_OPERATIONS.iter().any(|candidate| candidate.as_str() == operation) {
        return None;
    }
    if trailer(message, "Nyl-Local").is_some() {
        return None;
    }
    if trailer(message, "Nyl-Environment").is_some_and(|name| name != environment.as_str()) {
        return None;
    }
    RunId::new(trailer(message, "Nyl-Run-Id")?.trim()).ok()
}

/// The attestations recorded for one state, by scope and name: the newest
/// whose key names the state, however much later it was recorded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateAttestations {
    /// Per unit, the attestations keyed by the execution key of the unit's
    /// receipt in the state.
    pub units: BTreeMap<UnitName, BTreeMap<String, Attestation>>,
    /// Per unit whose receipt is not current in the state (its desired
    /// document changed and did not execute, such as a failed run), the
    /// attestations of the receipt it still has: what it published last, so
    /// what runs. Promotion reads them only to judge what runs now (D8);
    /// they never prove the state itself.
    pub stale_receipts: BTreeMap<UnitName, BTreeMap<String, Attestation>>,
    /// The environment-wide attestations keyed by the state's desired commit.
    pub environment: BTreeMap<String, Attestation>,
}

/// One recorded state with everything the promotion rules read.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedState {
    pub run: RunId,
    /// The state's commit on the desired ref, as `StateStore::state_commit`
    /// defines it: the key environment-wide attestations are compared with.
    pub desired_commit: CommitId,
    pub observed_commit: Option<CommitId>,
    /// The documents as of the state: environment record, desired and
    /// observed units, and artifacts. Its attestation fields are not read;
    /// [`Self::attestations`] holds what was recorded for the state.
    pub snapshot: EnvironmentSnapshot,
    /// What was recorded for the state, found by the shell by walking the
    /// observed history from the state's observed commit to the tip: a later
    /// `nyl attest` names this state's receipts or desired commit, and a
    /// later run drops them from the current files, but history keeps them.
    pub attestations: StateAttestations,
    /// What the drivers say the running-scoped attestations of each unit
    /// show, filled by the shell ([`super::running`]); empty when no driver
    /// was asked.
    pub running: BTreeMap<UnitName, super::running::UnitRunning>,
}

impl RecordedState {
    /// The source commit the state ran.
    pub fn source_commit(&self) -> Option<&CommitId> {
        self.snapshot.environment.as_ref().map(|record| &record.source_commit)
    }

    /// A state needs its environment record, and one that ran an archived
    /// worktree snapshot cannot establish clean promotion evidence.
    pub fn is_candidate(&self) -> bool {
        self.snapshot
            .environment
            .as_ref()
            .is_some_and(|record| !record.source_chosen_by.is_worktree())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_A: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90";
    const RUN_B: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b91";
    const RUN_C: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b92";

    fn entry(commit: char, operation: &str, run: &str, extra: &str) -> LogEntry {
        LogEntry {
            commit: CommitId::new(commit.to_string().repeat(40)).unwrap(),
            message: format!(
                "nyl: {operation} dev\n\nNyl-Operation: {operation}\nNyl-Run-Id: {run}\nNyl-Environment: dev\n{extra}"
            ),
        }
    }

    fn changed(entry: LogEntry) -> DesiredEntry {
        DesiredEntry {
            entry,
            changed_desired: true,
        }
    }

    fn unchanged(entry: LogEntry) -> DesiredEntry {
        DesiredEntry {
            entry,
            changed_desired: false,
        }
    }

    fn dev() -> EnvironmentName {
        EnvironmentName::new("dev").unwrap()
    }

    /// Catches a state that pairs the wrong observed commit, or one that
    /// counts an attestation, a promotion, another environment's run, or a
    /// local commit as evidence of what ran: only reconcile and recover
    /// transitions of this environment are states, paired by run.
    #[test]
    fn test_states_pair_commits_by_run_and_skip_other_operations() {
        let desired = [
            changed(entry('d', "promote", RUN_C, "")),
            changed(entry('c', "reconcile", RUN_B, "")),
            changed(entry('b', "reconcile", RUN_A, "Nyl-Local: true\n")),
            changed(entry('a', "reconcile", RUN_A, "")),
        ];
        let observed = [
            entry('4', "attest", RUN_C, ""),
            entry('3', "reconcile", RUN_B, ""),
            entry('1', "reconcile", RUN_A, ""),
        ];
        let states = recorded_states(&dev(), &desired, &observed);
        assert_eq!(
            states
                .iter()
                .map(|state| (
                    state.run.as_str(),
                    state.desired_commit.as_str().chars().next().unwrap(),
                ))
                .collect::<Vec<_>>(),
            [(RUN_B, 'c'), (RUN_A, 'a')]
        );
        assert_eq!(states[0].observed_commit.as_ref().unwrap().as_str(), "3".repeat(40));
        assert_eq!(states[1].observed_commit.as_ref().unwrap().as_str(), "1".repeat(40));
    }

    /// Catches another environment's run on a shared ref standing for this
    /// environment's state, and a shared ref pairing a commit with itself.
    #[test]
    fn test_states_of_another_environment_and_shared_refs() {
        let mut other = entry('b', "reconcile", RUN_B, "");
        other.message = other.message.replace("Nyl-Environment: dev", "Nyl-Environment: prod");
        let shared = [other, entry('a', "reconcile", RUN_A, "")];
        let desired: Vec<_> = shared.iter().cloned().map(changed).collect();
        let states = recorded_states(&dev(), &desired, &shared);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].desired_commit, states[0].observed_commit.clone().unwrap());
    }

    /// Catches an environment attestation keyed to a commit no state matches:
    /// on a shared ref, a run whose own commit changed only observed
    /// documents takes the state commit `StateStore::state_commit` names, the
    /// older commit that changed the desired documents, and a run with no
    /// such commit records no state.
    #[test]
    fn test_state_commit_is_the_newest_commit_that_changed_desired_documents() {
        let shared = [
            entry('c', "reconcile", RUN_B, ""),
            entry('b', "attest", RUN_C, ""),
            entry('a', "reconcile", RUN_A, ""),
        ];
        let desired = [
            unchanged(shared[0].clone()),
            unchanged(shared[1].clone()),
            changed(shared[2].clone()),
        ];
        let states = recorded_states(&dev(), &desired, &shared);
        assert_eq!(
            states
                .iter()
                .map(|state| (
                    state.run.as_str(),
                    state.desired_commit.as_str().chars().next().unwrap()
                ))
                .collect::<Vec<_>>(),
            [(RUN_B, 'a'), (RUN_A, 'a')],
            "the observed-only run B is attested under the commit of run A's state"
        );
        let nothing = recorded_states(&dev(), &[unchanged(shared[0].clone())], &shared[..1]);
        assert!(nothing.is_empty());
    }

    /// Catches a run with no observed commit being dropped: a state needs
    /// only its desired commit.
    #[test]
    fn test_a_run_without_an_observed_commit_still_records_a_state() {
        let states = recorded_states(&dev(), &[changed(entry('a', "recover", RUN_A, ""))], &[]);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].observed_commit, None);
    }
}
