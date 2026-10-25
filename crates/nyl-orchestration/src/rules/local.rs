//! Local state branches and `--local` runs.
//!
//! Contract: [Local runs](../../../../design/orchestration-core.md#local-runs)
//! and [Initialization and moves](../../../../design/orchestration-core.md#initialization-and-moves).
//!
//! - A `--local` run reads and writes the clone's local branches of the
//!   remote state refs, the local view of the location ([`view`]), and marks
//!   its commits `Nyl-Local: true`.
//! - A run against remote state refuses when a remote state ref's tip is a
//!   `Nyl-Local` commit, naming the last commit without the trailer
//!   ([`check_tip`], `NYL-STATE-LOCAL-TIP`).
//! - Diverged local branches are reported, such as "local state has 2 local
//!   transitions and is 3 behind the remote" ([`divergence_warning`]).
//! - `state reset --local` lists the units whose local receipts the remote
//!   does not reflect and proceeds only with `--discard <unit>…` or
//!   `--discard-all` ([`unreflected`], [`check_discard`]).
//! - `state restore --to <commit>` writes forward commits restoring that
//!   commit's content and never restores `admission.yaml`
//!   ([`restore_changes`]); it records the local transitions it rejects
//!   ([`rejected`]).
//! - A `--local` run refuses while the remote holds run refs of dead runs
//!   that remote state has not taken over yet ([`check_leftovers`],
//!   `NYL-LOCAL-RUN-LEFTOVERS`), because it never imports remote run refs;
//!   the lease it takes at the remote lists no units, since remote runs do
//!   not notice local effects.
//! - `state restore --to` pairs the other state ref through the transition
//!   trailers ([`restore_tips`]).
//! - A local run warns about every unit it executes, naming the remote
//!   command that records it again ([`unrecorded_warning`]), and runs units
//!   whose recovery policy is not `converge` only when named with
//!   `--approve` (decided in `rules::selection`).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{
    Coordination, EnvironmentSnapshot, LocalDivergence, LogEntry, StateSide, StateTips, TransitionChanges,
};
use nyl_core::orchestration::state::{ObservedUnit, StateLocation, StateRecord};
use nyl_core::orchestration::time::Timestamp;

use crate::rules::lease::run_live;
use crate::rules::state_location::describe;
use crate::transition::trailer;

/// The local view of a remote location: the same refs, path, and prefix in
/// the clone itself. `--local-lease` takes the lease here too.
///
/// `NYL-LOCAL-RUN-UNSUPPORTED` for a location that already is the clone's
/// own branches, and for state refs with full `refs/…` names outside
/// `refs/heads/`, which have no local branch.
pub fn view(location: &StateLocation) -> Result<StateLocation, Finding> {
    if location.is_local() {
        return Err(Finding::new(
            Code::LocalRunUnsupported,
            format!(
                "--local needs remote state, and the state at {} is this clone's own branches",
                describe(location)
            ),
            "run without --local: the local branches already are the state",
        ));
    }
    for configured in [&location.desired_ref, &location.observed_ref] {
        if !StateLocation::is_branch_ref(configured) {
            return Err(Finding::new(
                Code::LocalRunUnsupported,
                format!("state ref {configured} is a full ref name, which has no local branch in the clone"),
                "name desiredRef and observedRef as branches to work on local state, or run against the remote state",
            ));
        }
    }
    let mut view = location.clone();
    StateLocation::LOCAL_REPOSITORY.clone_into(&mut view.repository);
    Ok(view)
}

/// Whether a commit message carries the `Nyl-Local: true` trailer.
pub fn is_local_commit(message: &str) -> bool {
    trailer(message, "Nyl-Local").is_some_and(|value| value.trim() == "true")
}

/// Refuse when the newest of an environment's commits on a remote state
/// ref (its `log` filtered by [`environment_log`]) is a `Nyl-Local` commit,
/// naming the last commit without the trailer.
pub fn check_tip(environment: &EnvironmentName, side: StateSide, log: &[LogEntry]) -> Result<(), Finding> {
    let Some(tip) = log.first() else {
        return Ok(());
    };
    if !is_local_commit(&tip.message) {
        return Ok(());
    }
    let message = format!(
        "the tip {} of the {} state ref of {environment} is a Nyl-Local commit, which only a manual push of local state branches produces",
        tip.commit,
        side.as_str()
    );
    let fix = match log.iter().find(|entry| !is_local_commit(&entry.message)) {
        Some(last) => format!(
            "restore the last commit without the trailer with `nyl state restore -e {environment} --to {}`; pushed local transitions are never accepted",
            last.commit
        ),
        None => format!(
            "none of the last {} commits of {environment} on the ref is without the trailer; name the commit to restore with `nyl state restore -e {environment} --to <commit>`",
            log.len()
        ),
    };
    Err(Finding::new(Code::StateLocalTip, message, fix))
}

/// The local transitions on top of a remote state ref, newest first: what
/// `state restore` records as rejected.
pub fn rejected(log: &[LogEntry]) -> Vec<CommitId> {
    log.iter()
        .take_while(|entry| is_local_commit(&entry.message))
        .map(|entry| entry.commit.clone())
        .collect()
}

/// `1 local transition`, `2 local transitions`.
fn count(number: usize, noun: &str) -> String {
    if number == 1 {
        format!("1 {noun}")
    } else {
        format!("{number} {noun}s")
    }
}

/// The warning for diverged local branches; `None` when they follow the remote.
///
/// The counts are the larger of the two refs', since a transition writes at
/// most one commit per ref.
pub fn divergence_warning(environment: &EnvironmentName, divergence: &LocalDivergence) -> Option<Finding> {
    let sides = [divergence.desired, divergence.observed];
    let ahead = sides.iter().flatten().map(|side| side.ahead).max().unwrap_or(0);
    let behind = sides.iter().flatten().map(|side| side.behind).max().unwrap_or(0);
    let message = match (ahead, behind) {
        (0, 0) => return None,
        (ahead, 0) => format!(
            "local state of {environment} has {} the remote does not record",
            count(ahead, "local transition")
        ),
        (0, behind) => format!("local state of {environment} is {behind} behind the remote"),
        (ahead, behind) => format!(
            "local state of {environment} has {} and is {behind} behind the remote",
            count(ahead, "local transition")
        ),
    };
    let fix = if ahead == 0 {
        format!("read the remote state, such as with `nyl status -e {environment}`, to fast-forward the local branches")
    } else {
        format!(
            "`nyl status -e {environment} --local` shows the local view; once the remote records the change, follow it again with `nyl state reset -e {environment} --local`"
        )
    };
    Some(Finding::new(Code::StateLocalDiverged, message, fix))
}

/// Units whose receipt in the local view differs from the remote's: what
/// `state reset --local` would forget. A unit counts when it has a local
/// receipt and the remote has another receipt (execution key or uid), none,
/// or another condition.
pub fn unreflected(local: &EnvironmentSnapshot, remote: &EnvironmentSnapshot) -> Vec<UnitName> {
    let receipt = |unit: &ObservedUnit| {
        (
            unit.unit.uid.clone(),
            unit.receipt.as_ref().map(|receipt| receipt.execution_key.clone()),
        )
    };
    local
        .observed
        .iter()
        .filter(|(_, observed)| observed.receipt.is_some())
        .filter(|(unit, observed)| {
            remote
                .observed
                .get(*unit)
                .is_none_or(|theirs| receipt(observed) != receipt(theirs) || observed.condition != theirs.condition)
        })
        .map(|(unit, _)| unit.clone())
        .collect()
}

/// `--discard <unit>…` or `--discard-all`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discard {
    Units(BTreeSet<UnitName>),
    All,
}

/// Refuse a reset that would forget unreflected units not discarded
/// (`NYL-STATE-RESET-UNDISCARDED`).
pub fn check_discard(
    environment: &EnvironmentName,
    unreflected: &[UnitName],
    discard: Option<&Discard>,
) -> Result<(), Finding> {
    let kept = unreflected
        .iter()
        .filter(|unit| match discard {
            Some(Discard::All) => false,
            Some(Discard::Units(units)) => !units.contains(*unit),
            None => true,
        })
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if kept.is_empty() {
        return Ok(());
    }
    Err(Finding::new(
        Code::StateResetUndiscarded,
        format!(
            "resetting the local state of {environment} forgets local receipts the remote does not reflect: {}; their resources may be left behind",
            kept.join(", ")
        ),
        format!(
            "let the remote record them, such as with `nyl reconcile -e {environment}`, or undo their effects; then pass --discard {} or --discard-all",
            kept.join(",")
        ),
    ))
}

/// The changes of one per-unit document map: put what differs, remove what
/// `target` lacks.
fn replace<T: Clone + PartialEq>(
    current: &BTreeMap<UnitName, T>,
    target: &BTreeMap<UnitName, T>,
) -> BTreeMap<UnitName, Option<T>> {
    let mut changes = BTreeMap::new();
    for (unit, document) in target {
        if current.get(unit) != Some(document) {
            changes.insert(unit.clone(), Some(document.clone()));
        }
    }
    for unit in current.keys() {
        if !target.contains_key(unit) {
            changes.insert(unit.clone(), None);
        }
    }
    changes
}

/// The changes that make `current` hold `target`'s content: every document
/// of `target` put, every document absent from it removed, including
/// artifacts, observations, and attestations. `state.yaml` is carried
/// forward, never restored: the caller appends the `state-restored` event.
/// Admission is not part of a snapshot, so it is never restored.
pub fn restore_changes(current: &EnvironmentSnapshot, target: &EnvironmentSnapshot) -> TransitionChanges {
    TransitionChanges {
        record: None,
        environment: target
            .environment
            .as_ref()
            .filter(|environment| current.environment.as_ref() != Some(*environment))
            .cloned(),
        desired: replace(&current.desired, &target.desired),
        observed: replace(&current.observed, &target.observed),
        artifacts: replace(&current.artifacts, &target.artifacts),
        observations: replace(&current.observations, &target.observations),
        attestations: replace(&current.attestations, &target.attestations),
    }
}

/// `NYL-LOCAL-RUN-UNRECORDED` for a unit a `--local` run executed.
pub fn unrecorded_warning(environment: &EnvironmentName, unit: &UnitName) -> Finding {
    Finding::new(
        Code::LocalRunUnrecorded,
        format!(
            "{unit} executed in a --local run of {environment}; its effects are real, but the remote state does not record them"
        ),
        format!(
            "commit and push the change, then record it with `nyl reconcile -e {environment} --unit {unit}`; or undo the effects and drop the local transitions with `nyl state reset -e {environment} --local --discard {unit}`"
        ),
    )
}

/// Refuse a `--local` run (`NYL-LOCAL-RUN-LEFTOVERS`) when `coordination`
/// holds a foreign (remote) run ref of another run that remote state has to
/// take over: one that is finished, past its deadline, or whose run held a
/// broken lease (`nyl lease break` declares it gone, so the lease rule lets
/// a run take the lease at once). A local run never imports remote run refs,
/// so it would leave those units unmarked, and taking the broken lease would
/// erase the break that lets the next remote run take over. The fix names
/// `nyl reconcile -e <env>` against remote state. Other live foreign run
/// refs block through the lease rule.
///
/// The run shell checks before it takes the lease, so a refused run leaves
/// the remote lease and run refs as they were, and again once it holds it.
pub fn check_leftovers(
    environment: &EnvironmentName,
    run: &RunId,
    coordination: &Coordination,
    now: Timestamp,
) -> Result<(), Finding> {
    let dead = coordination
        .runs
        .iter()
        .filter(|state| state.foreign && state.run.run != *run && !run_live(coordination, state, now))
        .map(|state| state.run.run.to_string())
        .collect::<Vec<_>>();
    if dead.is_empty() {
        return Ok(());
    }
    let noun = if dead.len() == 1 { "run" } else { "runs" };
    let list = dead.join(", ");
    Err(Finding::new(
        Code::LocalRunLeftovers,
        format!(
            "the remote state of {environment} has run refs of dead {noun} {list}, which no run took over yet; a --local run never imports remote run refs"
        ),
        format!(
            "run `nyl reconcile -e {environment}` against remote state first, so it takes over {noun} {list}; then run with --local again"
        ),
    ))
}

/// The commit's `Nyl-Environment` trailer, which every transition commit
/// carries.
fn commit_environment(message: &str) -> Option<&str> {
    trailer(message, "Nyl-Environment").map(str::trim)
}

/// The entries of a state ref's `log` that belong to `environment`, newest
/// first: several environments may share one ref through `path`, so the tip
/// check, the rejected transitions, and `state restore` look only at the
/// environment's own commits, those whose `Nyl-Environment` trailer names it.
pub fn environment_log(environment: &EnvironmentName, log: &[LogEntry]) -> Vec<LogEntry> {
    log.iter()
        .filter(|entry| commit_environment(&entry.message) == Some(environment.as_str()))
        .cloned()
        .collect()
}

/// `NYL-STATE-MISSING` for remote state that is missing while the clone's
/// local branches of the same refs hold the environment's state, recorded
/// as local: a remote was added after the state started without one, and
/// `nyl state move --from local` takes it there. `None` when the local
/// branches hold no such state.
pub fn missing_on_remote(
    environment: &EnvironmentName,
    configured: &StateLocation,
    local_record: Option<&StateRecord>,
) -> Option<Finding> {
    let record = local_record?;
    if configured.is_local() || !record.location.is_local() || &record.environment != environment {
        return None;
    }
    Some(Finding::new(
        Code::StateMissing,
        format!(
            "{environment} has no state at {}, but this clone's local branches hold its state from before the state repository had a remote",
            describe(configured)
        ),
        format!(
            "move the state and its history to the remote with `nyl state move -e {environment} --from local`"
        ),
    ))
}

/// The tips `nyl state restore --to <to>` restores: `to` is a commit of
/// one side's log (newest first, as [`StateStore::log`](nyl_core::orchestration::ports::StateStore::log)
/// reads it). The other side is the commit carrying the same `Nyl-Run-Id`
/// trailer, else the one `to`'s `Nyl-Read-Desired`/`Nyl-Read-Observed`
/// trailer names. `NYL-STATE-RESTORE-INVALID` when `to` is in neither log,
/// when either chosen commit is a `Nyl-Local` commit, or when the other side
/// cannot be paired, or when `to` is a transition commit of another
/// environment sharing the refs. With one shared ref, both tips are `to`.
///
/// Both sides of one transition carry the same message, so among several
/// commits of one run on the other side (a move writes two), the one with
/// the identical message wins.
pub fn restore_tips(
    environment: &EnvironmentName,
    to: &CommitId,
    desired: &[LogEntry],
    observed: &[LogEntry],
    shared: bool,
) -> Result<StateTips, Finding> {
    let fix = format!(
        "name a commit without the Nyl-Local trailer from `git log` of a state ref of {environment}, such as the one the NYL-STATE-LOCAL-TIP refusal names"
    );
    let invalid = |message: String| Finding::new(Code::StateRestoreInvalid, message, fix.clone());
    let find = |log: &[LogEntry]| log.iter().find(|entry| entry.commit == *to).cloned();
    let (entry, side) = match (find(desired), find(observed)) {
        (Some(entry), _) => (entry, StateSide::Desired),
        (None, Some(entry)) => (entry, StateSide::Observed),
        (None, None) => {
            return Err(invalid(format!(
                "{to} is not among the read commits of the state refs of {environment}"
            )))
        }
    };
    let refuse_local = |entry: &LogEntry, what: &str| {
        if is_local_commit(&entry.message) {
            Err(invalid(format!(
                "{what} {} is a Nyl-Local commit; pushed local transitions are never restored",
                entry.commit
            )))
        } else {
            Ok(())
        }
    };
    if let Some(other) = commit_environment(&entry.message).filter(|name| *name != environment.as_str()) {
        return Err(invalid(format!(
            "{to} is a transition commit of environment {other}, not of {environment}"
        )));
    }
    refuse_local(&entry, "--to")?;
    if shared {
        return Ok(StateTips {
            desired: Some(to.clone()),
            observed: Some(to.clone()),
        });
    }
    let (other_log, other_side, read_trailer) = match side {
        StateSide::Desired => (observed, StateSide::Observed, "Nyl-Read-Observed"),
        StateSide::Observed => (desired, StateSide::Desired, "Nyl-Read-Desired"),
    };
    let same_run = trailer(&entry.message, "Nyl-Run-Id").and_then(|run| {
        let candidates = other_log
            .iter()
            .filter(|candidate| trailer(&candidate.message, "Nyl-Run-Id") == Some(run))
            .collect::<Vec<_>>();
        candidates
            .iter()
            .find(|candidate| candidate.message == entry.message)
            .or_else(|| candidates.first())
            .map(|candidate| (*candidate).clone())
    });
    let other = match (same_run, trailer(&entry.message, read_trailer)) {
        (Some(paired), _) => Some(paired),
        (None, Some(read)) => match other_log.iter().find(|candidate| candidate.commit.as_str() == read.trim()) {
            Some(found) => Some(found.clone()),
            None => {
                return Err(invalid(format!(
                    "{to} read {read} from the {} state ref of {environment}, which is not among its read commits",
                    other_side.as_str()
                )))
            }
        },
        // The other ref had no commit yet when `to` was written.
        (None, None) if other_log.is_empty() => None,
        (None, None) => {
            return Err(invalid(format!(
                "{to} has no {side} commit of the same run and names none it read (no {read_trailer} trailer), so the {side} state to restore with it is unknown",
                side = other_side.as_str()
            )))
        }
    };
    if let Some(other) = &other {
        refuse_local(other, &format!("the paired {} commit", other_side.as_str()))?;
    }
    let other = other.map(|entry| entry.commit);
    Ok(match side {
        StateSide::Desired => StateTips {
            desired: Some(to.clone()),
            observed: other,
        },
        StateSide::Observed => StateTips {
            desired: other,
            observed: Some(to.clone()),
        },
    })
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::ports::{Divergence, HeldLease, RunState};
    use nyl_core::orchestration::state::{Run, StateRecord};
    use serde_json::json;

    use super::*;
    use crate::rules::fixtures::{
        self, env as dev, hex_key, lease_record, name as unit, observed_numbered, run_record,
    };

    const RUN_A: &str = "00000000-0000-7000-9000-00000000000a";
    const RUN_B: &str = "00000000-0000-7000-9000-00000000000b";
    const RUN_C: &str = "00000000-0000-7000-9000-00000000000c";

    fn commit(digit: char) -> CommitId {
        CommitId::new(digit.to_string().repeat(40)).unwrap()
    }

    /// A transition message with `run` and the read trailers, local or not.
    fn message(run: &str, read: Option<(char, char)>, local: bool) -> String {
        let mut text = format!("nyl: reconcile dev\n\nunits: {{}}\n\nNyl-Operation: reconcile\nNyl-Run-Id: {run}\n");
        if let Some((desired, observed)) = read {
            for (key, digit) in [("Nyl-Read-Desired", desired), ("Nyl-Read-Observed", observed)] {
                text.push_str(key);
                text.push_str(": ");
                text.push_str(commit(digit).as_str());
                text.push('\n');
            }
        }
        if local {
            text.push_str("Nyl-Local: true\n");
        }
        text
    }

    fn entry(digit: char, message: String) -> LogEntry {
        LogEntry {
            commit: commit(digit),
            message,
        }
    }

    fn remote() -> StateLocation {
        StateLocation {
            repository: "https://example.com/state.git".to_owned(),
            desired_ref: "nyl/dev/desired".to_owned(),
            observed_ref: "nyl/dev/observed".to_owned(),
            path: "envs/dev".to_owned(),
            coordination_ref_prefix: "refs/nyl/".to_owned(),
        }
    }

    /// Catches a `--local` run reading the remote refs, or a view of refs
    /// the clone has no branch for.
    #[test]
    fn test_view_keeps_refs_in_the_clone_and_refuses_full_ref_names() {
        let view = view(&remote()).unwrap();
        assert_eq!(view.repository, StateLocation::LOCAL_REPOSITORY);
        assert_eq!(
            (
                view.desired_ref.as_str(),
                view.path.as_str(),
                view.coordination_ref_prefix.as_str()
            ),
            ("nyl/dev/desired", "envs/dev", "refs/nyl/")
        );
        let mut full = remote();
        full.observed_ref = "refs/nyl/dev/observed".to_owned();
        assert_eq!(super::view(&full).unwrap_err().code, Code::LocalRunUnsupported);
        let mut heads = remote();
        heads.desired_ref = "refs/heads/nyl/dev/desired".to_owned();
        assert!(super::view(&heads).is_ok());
        assert_eq!(super::view(&view).unwrap_err().code, Code::LocalRunUnsupported);
    }

    /// Catches a run continuing on pushed local transitions, or a fix that
    /// names a commit restore would refuse.
    #[test]
    fn test_check_tip_refuses_a_local_tip_naming_the_last_remote_commit() {
        let ordinary = entry('1', message(RUN_A, None, false));
        assert!(check_tip(&dev(), StateSide::Desired, std::slice::from_ref(&ordinary)).is_ok());
        assert!(check_tip(&dev(), StateSide::Desired, &[]).is_ok());
        let log = [
            entry('3', message(RUN_C, None, true)),
            entry('2', message(RUN_B, None, true)),
            ordinary,
        ];
        let finding = check_tip(&dev(), StateSide::Observed, &log).unwrap_err();
        assert_eq!(finding.code, Code::StateLocalTip);
        assert!(finding.message.contains("observed"), "{}", finding.message);
        assert!(
            finding
                .fix
                .contains(&format!("nyl state restore -e dev --to {}", commit('1'))),
            "{}",
            finding.fix
        );
        let all_local = check_tip(&dev(), StateSide::Desired, &log[..2]).unwrap_err();
        assert!(all_local.fix.contains("--to <commit>"), "{}", all_local.fix);
        assert_eq!(rejected(&log), vec![commit('3'), commit('2')]);
        assert!(rejected(&log[2..]).is_empty());
    }

    /// Catches a silent divergence or a warning while the branches follow
    /// the remote.
    #[test]
    fn test_divergence_warning_counts_local_transitions_and_commits_behind() {
        assert_eq!(divergence_warning(&dev(), &LocalDivergence::default()), None);
        let even = Divergence { ahead: 0, behind: 0 };
        assert_eq!(
            divergence_warning(
                &dev(),
                &LocalDivergence {
                    desired: Some(even),
                    observed: Some(even)
                }
            ),
            None
        );
        let warning = divergence_warning(
            &dev(),
            &LocalDivergence {
                desired: Some(Divergence { ahead: 1, behind: 3 }),
                observed: Some(Divergence { ahead: 2, behind: 1 }),
            },
        )
        .unwrap();
        assert_eq!(warning.code, Code::StateLocalDiverged);
        assert_eq!(
            warning.message,
            "local state of dev has 2 local transitions and is 3 behind the remote"
        );
        assert!(
            warning.fix.contains("nyl state reset -e dev --local"),
            "{}",
            warning.fix
        );
        let ahead = divergence_warning(
            &dev(),
            &LocalDivergence {
                desired: Some(Divergence { ahead: 1, behind: 0 }),
                observed: None,
            },
        )
        .unwrap();
        assert_eq!(
            ahead.message,
            "local state of dev has 1 local transition the remote does not record"
        );
    }

    fn observed(name: &str, uid: u64, key: char, condition: Option<&serde_json::Value>) -> ObservedUnit {
        let mut document = observed_numbered(name, uid, Some(hex_key(key)));
        document.condition = condition.map(|condition| serde_json::from_value(condition.clone()).unwrap());
        document
    }

    fn snapshot(units: Vec<ObservedUnit>) -> EnvironmentSnapshot {
        EnvironmentSnapshot {
            observed: units
                .into_iter()
                .map(|observed| (observed.unit.name.clone(), observed))
                .collect(),
            ..EnvironmentSnapshot::default()
        }
    }

    /// Catches a reset that forgets local receipts without asking, or one
    /// that asks about units the remote reflects.
    #[test]
    fn test_unreflected_lists_local_receipts_the_remote_lacks_and_discard_covers_them() {
        let uncertain = json!({"state": "uncertain", "since": "2026-09-24T10:02:00Z", "run": RUN_B, "reason": "lost"});
        let local = snapshot(vec![
            observed("same", 1, 'a', None),
            observed("rekeyed", 1, 'b', None),
            observed("reincarnated", 2, 'a', None),
            observed("new", 1, 'a', None),
            observed("conditioned", 1, 'a', Some(&uncertain)),
        ]);
        let remote = snapshot(vec![
            observed("same", 1, 'a', None),
            observed("rekeyed", 1, 'a', None),
            observed("reincarnated", 1, 'a', None),
            observed("conditioned", 1, 'a', None),
        ]);
        let units = unreflected(&local, &remote);
        assert_eq!(
            units,
            vec![unit("conditioned"), unit("new"), unit("reincarnated"), unit("rekeyed")]
        );
        let refused = check_discard(&dev(), &units, None).unwrap_err();
        assert_eq!(refused.code, Code::StateResetUndiscarded);
        assert!(refused.message.contains("conditioned, new, reincarnated, rekeyed"));
        let partly = Discard::Units(BTreeSet::from([unit("new"), unit("rekeyed")]));
        let refused = check_discard(&dev(), &units, Some(&partly)).unwrap_err();
        assert!(
            refused.fix.contains("--discard conditioned,reincarnated"),
            "{}",
            refused.fix
        );
        assert!(check_discard(&dev(), &units, Some(&Discard::All)).is_ok());
        assert!(check_discard(&dev(), &[], None).is_ok());
    }

    /// Catches a restore that leaves units the target lacks, rewrites
    /// unchanged documents, or restores `state.yaml`.
    #[test]
    fn test_restore_changes_replace_content_and_never_the_record() {
        let record: StateRecord = serde_json::from_value(json!({
            "apiVersion": "gitops.nyl/v1", "kind": "StateRecord", "environment": "dev",
            "location": serde_json::to_value(remote()).unwrap(), "events": []
        }))
        .unwrap();
        let mut current = snapshot(vec![
            observed("kept", 1, 'a', None),
            observed("changed", 1, 'b', None),
            observed("added-locally", 1, 'a', None),
        ]);
        current.record = Some(record);
        let mut target = snapshot(vec![observed("kept", 1, 'a', None), observed("changed", 1, 'a', None)]);
        target.artifacts.insert(unit("changed"), BTreeMap::new());
        current.artifacts.insert(unit("added-locally"), BTreeMap::new());
        let changes = restore_changes(&current, &target);
        assert_eq!(changes.record, None);
        assert_eq!(
            changes.observed.keys().cloned().collect::<Vec<_>>(),
            vec![unit("added-locally"), unit("changed")]
        );
        assert_eq!(changes.observed[&unit("added-locally")], None);
        assert_eq!(
            changes.observed[&unit("changed")].as_ref(),
            target.observed.get(&unit("changed"))
        );
        assert_eq!(changes.artifacts[&unit("added-locally")], None);
        assert_eq!(changes.artifacts[&unit("changed")], Some(BTreeMap::new()));
        assert!(restore_changes(&target, &target).is_empty());
    }

    /// Catches restoring a desired and observed pair that never existed
    /// together, or restoring a pushed local transition.
    #[test]
    fn test_restore_tips_pairs_by_run_then_by_read_trailer() {
        // Desired: 5 (run B, local) on 3 (run B), on 1 (run A).
        // Observed: 6 (local), 4 (run C read desired 3 / observed 2), 2 (run A).
        let desired = [
            entry('5', message(RUN_B, None, true)),
            entry('3', message(RUN_B, Some(('1', '2')), false)),
            entry('1', message(RUN_A, None, false)),
        ];
        let observed = [
            entry('6', message(RUN_C, None, true)),
            entry('4', message(RUN_C, Some(('3', '2')), false)),
            entry('2', message(RUN_A, None, false)),
        ];
        // Same run on both sides.
        assert_eq!(
            restore_tips(&dev(), &commit('1'), &desired, &observed, false).unwrap(),
            StateTips {
                desired: Some(commit('1')),
                observed: Some(commit('2'))
            }
        );
        // No observed commit of run B: the read trailer names it.
        assert_eq!(
            restore_tips(&dev(), &commit('3'), &desired, &observed, false).unwrap(),
            StateTips {
                desired: Some(commit('3')),
                observed: Some(commit('2'))
            }
        );
        // From the observed side.
        assert_eq!(
            restore_tips(&dev(), &commit('4'), &desired, &observed, false).unwrap(),
            StateTips {
                desired: Some(commit('3')),
                observed: Some(commit('4'))
            }
        );
        for refused in ['5', '6', '9'] {
            let finding = restore_tips(&dev(), &commit(refused), &desired, &observed, false).unwrap_err();
            assert_eq!(finding.code, Code::StateRestoreInvalid, "{refused}");
        }
        // The read trailer names a commit outside the log.
        let unknown = [entry('7', message(RUN_B, Some(('1', '8')), false))];
        let finding = restore_tips(&dev(), &commit('7'), &unknown, &observed, false).unwrap_err();
        assert!(
            finding.message.contains(&commit('8').to_string()),
            "{}",
            finding.message
        );
        // A paired commit that is local is refused too.
        let paired_local = [entry('8', message(RUN_C, Some(('3', '2')), true))];
        let finding = restore_tips(&dev(), &commit('4'), &paired_local, &observed[1..], false);
        assert!(finding.is_err());
        // One shared ref.
        assert_eq!(
            restore_tips(&dev(), &commit('3'), &desired, &desired, true).unwrap(),
            StateTips {
                desired: Some(commit('3')),
                observed: Some(commit('3'))
            }
        );
    }

    fn run_state(run: &str, deadline: &str, finished: bool, foreign: bool) -> RunState {
        RunState {
            foreign,
            ..fixtures::run_state(Run {
                finished,
                ..run_record(run, deadline.parse().unwrap())
            })
        }
    }

    /// Catches a local run proceeding while remote state has dead runs to
    /// take over, or refusing because of live or own run refs.
    #[test]
    fn test_check_leftovers_refuses_dead_foreign_runs_only() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        let own = RunId::new(RUN_A).unwrap();
        let passing = Coordination {
            lease: None::<HeldLease>,
            runs: vec![
                run_state(RUN_A, "2026-09-24T09:00:00Z", false, true),
                run_state(RUN_B, "2026-09-24T11:00:00Z", false, true),
                run_state(RUN_C, "2026-09-24T09:00:00Z", true, false),
            ],
            evidence: Vec::new(),
        };
        assert!(check_leftovers(&dev(), &own, &passing, now).is_ok());
        for dead in [
            run_state(RUN_C, "2026-09-24T09:00:00Z", false, true),
            run_state(RUN_C, "2026-09-24T11:00:00Z", true, true),
        ] {
            let mut coordination = passing.clone();
            coordination.runs.push(dead);
            let finding = check_leftovers(&dev(), &own, &coordination, now).unwrap_err();
            assert_eq!(finding.code, Code::LocalRunLeftovers);
            assert!(finding.message.contains(RUN_C), "{}", finding.message);
            assert!(finding.fix.contains("nyl reconcile -e dev"), "{}", finding.fix);
        }
    }

    /// Catches a `Nyl-Local` trailer of another value counting as local.
    #[test]
    fn test_is_local_commit_reads_the_trailer() {
        assert!(is_local_commit(&message(RUN_A, None, true)));
        assert!(!is_local_commit(&message(RUN_A, None, false)));
        assert!(!is_local_commit("nyl: reconcile dev\n\nNyl-Local: false\n"));
    }

    /// Catches a `--local` run taking a broken remote lease while the broken
    /// holder's run ref is still within its deadline: it would never import
    /// that run ref, and would erase the break the next remote run needs.
    #[test]
    fn test_check_leftovers_refuses_the_broken_holders_live_run() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
            .unwrap()
            .to_utc();
        let mut held = HeldLease {
            lease: lease_record(RUN_C, "2026-09-24T11:00:00Z".parse().unwrap()),
            version: "v1".to_owned(),
        };
        let live = Coordination {
            lease: Some(held.clone()),
            runs: vec![run_state(RUN_C, "2026-09-24T11:00:00Z", false, true)],
            evidence: Vec::new(),
        };
        let own = RunId::new(RUN_A).unwrap();
        // Not broken: the lease rule blocks; nothing is left over.
        assert!(check_leftovers(&dev(), &own, &live, now).is_ok());
        held.lease.broken = Some(serde_json::from_value(json!({"by": "ops", "at": "2026-09-24T09:30:00Z"})).unwrap());
        let broken = Coordination {
            lease: Some(held),
            ..live
        };
        let finding = check_leftovers(&dev(), &own, &broken, now).unwrap_err();
        assert_eq!(finding.code, Code::LocalRunLeftovers);
        assert!(finding.message.contains(RUN_C), "{}", finding.message);
    }

    fn commit_of(environment: &str, digit: char, run: &str, local: bool) -> LogEntry {
        let mut text = message(run, None, local);
        text.push_str("Nyl-Environment: ");
        text.push_str(environment);
        text.push('\n');
        entry(digit, text)
    }

    /// Catches environments sharing refs through `path` judging, rejecting,
    /// or restoring each other's commits.
    #[test]
    fn test_environment_log_and_restore_keep_to_the_environments_commits() {
        let log = vec![
            commit_of("preview", '3', RUN_C, true),
            commit_of("dev", '2', RUN_B, true),
            commit_of("preview", '1', RUN_A, false),
        ];
        let own = environment_log(&dev(), &log);
        assert_eq!(
            own.iter().map(|entry| entry.commit.clone()).collect::<Vec<_>>(),
            vec![commit('2')]
        );
        assert_eq!(rejected(&own), vec![commit('2')]);
        assert!(check_tip(&dev(), StateSide::Desired, &own).is_err());
        let preview = EnvironmentName::new("preview").unwrap();
        assert_eq!(rejected(&environment_log(&preview, &log)), vec![commit('3')]);

        let finding = restore_tips(&dev(), &commit('1'), &log, &log, true).unwrap_err();
        assert_eq!(finding.code, Code::StateRestoreInvalid);
        assert!(finding.message.contains("environment preview"), "{}", finding.message);
        assert!(restore_tips(&preview, &commit('1'), &log, &log, true).is_ok());
    }

    /// Catches a run against a newly added remote pointing at an unknown
    /// old location when the state is on the clone's local branches.
    #[test]
    fn test_missing_on_remote_names_the_move_from_local() {
        let mut local = remote();
        StateLocation::LOCAL_REPOSITORY.clone_into(&mut local.repository);
        let record = |location: &StateLocation, environment: &str| -> StateRecord {
            serde_json::from_value(json!({
                "apiVersion": "gitops.nyl/v1", "kind": "StateRecord", "environment": environment,
                "location": serde_json::to_value(location).unwrap(), "events": []
            }))
            .unwrap()
        };
        let finding = missing_on_remote(&dev(), &remote(), Some(&record(&local, "dev"))).unwrap();
        assert_eq!(finding.code, Code::StateMissing);
        assert_eq!(
            finding.fix,
            "move the state and its history to the remote with `nyl state move -e dev --from local`"
        );
        assert!(missing_on_remote(&dev(), &remote(), None).is_none());
        assert!(missing_on_remote(&dev(), &remote(), Some(&record(&remote(), "dev"))).is_none());
        assert!(missing_on_remote(&dev(), &remote(), Some(&record(&local, "preview"))).is_none());
    }
}
