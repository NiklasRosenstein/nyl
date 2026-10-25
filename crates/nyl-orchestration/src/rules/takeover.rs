//! Run takeover: what a run that took over an expired or broken lease
//! imports from the leftover run refs.
//!
//! Contract: [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases),
//! items 3 (checkpoints), 4 (takeover), 6 (lost lease), 7 (cancellation),
//! and 8 (breaking a lease), and the walkthrough
//! [Effects without a receipt](../../../../design/orchestration-core.md#walkthroughs).
//!
//! - Before desired documents are built, the run adopts the checkpointed or
//!   recorded desired document and uid of every unit a leftover run created
//!   and started, with or without a checkpoint ([`adoptions`]); a unit no
//!   longer selected then leaves the ownership set like any other.
//! - After resolving, a checkpointed result becomes the unit's receipt (or
//!   condition) when the new resolution has the same execution key, and the
//!   unit is listed under `imported` ([`import`]).
//! - When several leftover runs touched a unit (checkpointed it, or started
//!   it without a checkpoint), the newest run (by deadline, then run id)
//!   decides; older checkpoints are reported as superseded.
//! - Other checkpointed results are superseded: reported with
//!   `NYL-RUN-SUPERSEDED`. A superseded result whose effects are real (a
//!   receipt, or an `uncertain` outcome) with an execution key other than
//!   the unit's current receipt's gives the unit an `uncertain` condition,
//!   whatever its recovery policy; a converging unit then runs again.
//! - Units in a leftover run's `units` (or the replaced lease's) without a
//!   checkpoint get an `uncertain` condition, such as "run lost its lease";
//!   they are listed under `recovered`.
//! - A checkpoint of a unit that is already `uncertain` is a late checkpoint:
//!   evidence only (`NYL-RUN-LATE-CHECKPOINT`), never clearing the condition.
//! - A run whose lease was broken may still be alive and write to its run
//!   ref after the takeover read it. Consumption therefore acts only on refs
//!   unchanged since that read ([`unchanged`]); a changed one is left for
//!   the next run, which imports what arrived (as a late checkpoint where
//!   the unit is `uncertain`).
//! - Import is idempotent and never regresses state: a takeover condition
//!   records the dead run's id, and a unit whose condition already names
//!   that run is left alone; a checkpoint applies only when it is newer than
//!   the unit's observed document, and a unit the dead run started is not
//!   marked when its observed document changed at or after that run's
//!   deadline; a (run, unit) pair is handled once per run
//!   ([`ImportInputs::handled`]).
//! - Adopted units the new resolution no longer selects are imported too:
//!   their checkpoint becomes their receipt (there is no new key to compare)
//!   and, without one, they become `uncertain`, so the transition commit
//!   records state for every resource a dead run created.
//! - A `--local` run never imports, retires, or deletes run refs it read
//!   from the remote ([`RunState::foreign`]), and a replaced lease counts only
//!   when its operation has effects ([`leftovers`]).
//! - Every change goes through [`crate::rules::transitions::apply`]. After
//!   the transition commit lands, [`consumed`] decides which leftover run
//!   refs are deleted and which are retired to evidence refs
//!   (`<prefix><env>/evidence/<run>`), because a checkpointed receipt on them
//!   is still evidence for an `uncertain` unit that `nyl recover --accept`
//!   may adopt. Evidence refs no `uncertain` unit needs are deleted.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::ports::{Checkpoint, Coordination, RunState};
use nyl_core::orchestration::state::{
    ArtifactDocument, Condition, DesiredUnit, Lease, ObservedUnit, Operation, Receipt,
};
use nyl_core::orchestration::time::Timestamp;

use super::transitions::{apply, Applied, Event};

/// What a run that took over the lease found of the runs before it.
#[derive(Debug, Clone)]
pub struct Leftovers<'a> {
    /// The run taking over; its own run ref is never a leftover.
    pub run: &'a RunId,
    /// The expired or broken lease it replaced, when its operation has
    /// effects (reconcile, teardown, exec) and the run may take its `units`
    /// as started; `None` otherwise, and always for a `--local` run that
    /// took the remote lease.
    pub replaced: Option<&'a Lease>,
    /// The run refs (`<prefix><env>/runs/*`) to import: every one read after
    /// taking the lease, except the run's own and foreign ones.
    pub runs: Vec<&'a RunState>,
    /// Retired run refs (`<prefix><env>/evidence/*`): never imported again,
    /// only consulted for late checkpoints and deleted once no unit needs them.
    pub evidence: Vec<&'a RunState>,
}

/// Whether a replaced lease of `operation` may have left units half-executed.
fn has_effects(operation: Operation) -> bool {
    matches!(operation, Operation::Reconcile | Operation::Teardown | Operation::Exec)
}

/// The leftovers of `coordination` for `run`: drops the run's own ref and
/// foreign ones, and keeps `replaced` only for an operation with effects.
pub fn leftovers<'a>(run: &'a RunId, replaced: Option<&'a Lease>, coordination: &'a Coordination) -> Leftovers<'a> {
    let theirs = |state: &&RunState| &state.run.run != run && !state.foreign;
    Leftovers {
        run,
        replaced: replaced.filter(|lease| has_effects(lease.operation) && &lease.run != run),
        runs: coordination.runs.iter().filter(theirs).collect(),
        evidence: coordination.evidence.iter().filter(theirs).collect(),
    }
}

/// Leftover runs ordered so that the newest, by deadline and then run id,
/// comes last.
fn oldest_first<'a>(states: &[&'a RunState]) -> Vec<&'a RunState> {
    let mut states = states.to_vec();
    states.sort_by(|a, b| (a.run.deadline, &a.run.run).cmp(&(b.run.deadline, &b.run.run)));
    states
}

/// The desired documents, with their uids, to adopt for units leftover runs
/// created and started (their checkpoints' `desired` documents and
/// [`RunState::created`]), by unit. The shell overlays them on the
/// snapshot's desired documents passed to `rules::desired`, so their uids
/// are reused, but always diffs its transition against the real snapshot.
/// The shell drops the documents whose uid the snapshot already has. When
/// several runs recorded a unit, the newest run's document wins, and a
/// run's checkpoint wins over its record of the start.
pub fn adoptions(leftovers: &Leftovers<'_>) -> BTreeMap<UnitName, DesiredUnit> {
    let mut adopted = BTreeMap::new();
    for state in oldest_first(&leftovers.runs) {
        let documents = state
            .created
            .iter()
            .chain(state.checkpoints.iter().map(|checkpoint| &checkpoint.desired));
        for desired in documents.filter(|desired| desired.unit.uid.is_some()) {
            adopted.insert(desired.unit.name.clone(), desired.clone());
        }
    }
    adopted
}

/// What import reads besides the leftovers.
pub struct ImportInputs<'a> {
    pub leftovers: &'a Leftovers<'a>,
    /// The environment, for the `nyl recover` commands warnings name.
    pub environment: &'a EnvironmentName,
    /// The time conditions are recorded at.
    pub now: Timestamp,
    /// The units as this run resolved them, with execution keys where resolved.
    pub resolved: &'a BTreeMap<UnitName, DesiredUnit>,
    /// Units the run does not resolve: adopted units no longer selected, and
    /// leaving units with their stored desired documents. Their checkpoints
    /// are imported without a key comparison. After the last wave the shell
    /// also passes resolved units still without an execution key here, so
    /// no checkpoint stays unimported.
    pub adopted: &'a BTreeMap<UnitName, DesiredUnit>,
    /// The observed documents as the run's snapshot read them.
    pub snapshot: &'a BTreeMap<UnitName, ObservedUnit>,
    /// The snapshot's observed documents plus everything imported so far.
    pub observed: &'a BTreeMap<UnitName, ObservedUnit>,
    /// The (run, unit) pairs earlier calls in this run already handled
    /// (imported, superseded, marked uncertain, or reported as late), which
    /// this call neither applies nor reclassifies again.
    pub handled: &'a BTreeSet<(RunId, UnitName)>,
    /// The kinds' drivers. Takeover itself decides without recovery
    /// policies (a superseded result is uncertain whatever the policy);
    /// selection applies them afterwards.
    pub drivers: &'a DriverSet,
}

/// What a takeover imports into the run.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Import {
    /// New observed documents, overlaid on the run's evidence and written in
    /// its transition commit.
    pub observed: BTreeMap<UnitName, ObservedUnit>,
    /// The artifact sets of imported receipts.
    pub artifacts: BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    /// Units whose checkpointed result became their receipt or condition:
    /// the summary's `imported`.
    pub imported: Vec<UnitName>,
    /// Units given an `uncertain` condition by the takeover (started without
    /// a checkpoint, or a superseded result): the summary's `recovered`,
    /// with the units the run then recovers by executing or inspecting.
    pub recovered: Vec<UnitName>,
    /// `NYL-RUN-SUPERSEDED` and `NYL-RUN-LATE-CHECKPOINT` warnings.
    pub warnings: Vec<Finding>,
    /// The (run, unit) pairs this call handled, for the shell to accumulate.
    pub handled: BTreeSet<(RunId, UnitName)>,
}

/// How a leftover run touched a unit.
#[derive(Clone, Copy)]
enum Touch<'a> {
    /// It finished the unit and checkpointed the result.
    Checkpointed(&'a Checkpoint),
    /// It started the unit and left no checkpoint; `broken` when its lease
    /// was broken.
    Started { broken: bool },
}

/// One leftover run's touch of one unit.
#[derive(Clone, Copy)]
struct Touched<'a> {
    run: &'a RunId,
    deadline: Timestamp,
    touch: Touch<'a>,
}

impl Touched<'_> {
    fn recency(&self) -> (Timestamp, &RunId) {
        (self.deadline, self.run)
    }
}

/// Every unit's touches by leftover runs, newest first.
fn touches<'a>(leftovers: &Leftovers<'a>) -> BTreeMap<UnitName, Vec<Touched<'a>>> {
    let mut touched: BTreeMap<UnitName, Vec<Touched<'a>>> = BTreeMap::new();
    let mut replaced_has_ref = false;
    for state in &leftovers.runs {
        let run = &state.run.run;
        let deadline = state.run.deadline;
        let mut last: BTreeMap<&UnitName, &Checkpoint> = BTreeMap::new();
        for checkpoint in &state.checkpoints {
            last.insert(&checkpoint.desired.unit.name, checkpoint);
        }
        let mut started: BTreeSet<&UnitName> = state.run.units.iter().collect();
        let mut broken = false;
        if let Some(lease) = leftovers.replaced.filter(|lease| &lease.run == run) {
            replaced_has_ref = true;
            broken = lease.broken.is_some();
            started.extend(&lease.units);
        }
        for (unit, checkpoint) in &last {
            touched.entry((*unit).clone()).or_default().push(Touched {
                run,
                deadline,
                touch: Touch::Checkpointed(checkpoint),
            });
        }
        for unit in started.into_iter().filter(|unit| !last.contains_key(unit)) {
            touched.entry(unit.clone()).or_default().push(Touched {
                run,
                deadline,
                touch: Touch::Started { broken },
            });
        }
    }
    if let (Some(lease), false) = (leftovers.replaced, replaced_has_ref) {
        for unit in &lease.units {
            touched.entry(unit.clone()).or_default().push(Touched {
                run: &lease.run,
                deadline: lease.deadline,
                touch: Touch::Started {
                    broken: lease.broken.is_some(),
                },
            });
        }
    }
    for list in touched.values_mut() {
        list.sort_by(|a, b| b.recency().cmp(&a.recency()));
    }
    touched
}

/// The time an observed document last changed: its condition's, else its
/// receipt's finish.
fn changed_at(observed: &ObservedUnit) -> Option<Timestamp> {
    match &observed.condition {
        Some(Condition::Failed { since, .. } | Condition::Uncertain { since, .. }) => Some(*since),
        None => observed.receipt.as_ref().map(|receipt| receipt.finished_at),
    }
}

/// The run of an observed document's latest change.
fn changed_by(observed: &ObservedUnit) -> Option<&RunId> {
    match &observed.condition {
        Some(Condition::Failed { run, .. } | Condition::Uncertain { run, .. }) => Some(run),
        None => observed.receipt.as_ref().map(|receipt| &receipt.run),
    }
}

/// Whether the snapshot's observed document already holds `checkpoint` of
/// `run`, or something newer: a re-read leftover is then a no-op.
fn not_newer(snapshot: Option<&ObservedUnit>, run: &RunId, checkpoint: &Checkpoint) -> bool {
    let Some(snapshot) = snapshot.filter(|snapshot| snapshot.unit.uid == checkpoint.observed.unit.uid) else {
        return false;
    };
    let (Some(stored), Some(checked)) = (changed_at(snapshot), changed_at(&checkpoint.observed)) else {
        return false;
    };
    stored > checked || (stored == checked && changed_by(snapshot) == Some(run))
}

/// The receipt a checkpoint recorded for its own run, if it is a success.
fn checkpointed_receipt<'c>(run: &RunId, checkpoint: &'c Checkpoint) -> Option<&'c Receipt> {
    checkpoint
        .observed
        .receipt
        .as_ref()
        .filter(|receipt| checkpoint.observed.condition.is_none() && &receipt.run == run)
}

/// Whether a checkpoint shows effects that happened: a receipt the run
/// recorded, or an `uncertain` outcome. A failure claims none.
fn effects_real(run: &RunId, checkpoint: &Checkpoint) -> bool {
    match &checkpoint.observed.condition {
        Some(Condition::Uncertain { .. }) => true,
        Some(Condition::Failed { .. }) => false,
        None => checkpointed_receipt(run, checkpoint).is_some(),
    }
}

fn superseded_warning(environment: &EnvironmentName, unit: &UnitName, run: &RunId, why: &str) -> Finding {
    Finding::new(
        Code::RunSuperseded,
        format!("the result run {run} checkpointed for unit {unit} is superseded: {why}"),
        format!(
            "nothing to do when the unit converges; otherwise check its effects and run \
             `nyl recover -e {environment} --unit {unit} --retry`"
        ),
    )
}

fn late_warning(environment: &EnvironmentName, unit: &UnitName, run: &RunId) -> Finding {
    Finding::new(
        Code::RunLateCheckpoint,
        format!(
            "run {run} checkpointed unit {unit} after it was marked uncertain; the checkpoint is kept as evidence and \
             does not clear the condition"
        ),
        format!(
            "check the result, then adopt it with `nyl recover -e {environment} --unit {unit} --accept {run} --reason \
             <text>`, or run the unit again with `nyl recover -e {environment} --unit {unit} --retry`"
        ),
    )
}

/// The reason of the condition a takeover gives a unit `run` started.
fn started_reason(run: &RunId, broken: bool) -> String {
    if broken {
        format!("run {run} was executing it when its lease was broken")
    } else {
        format!("run {run} lost its lease while executing it")
    }
}

/// What import does with one unit.
enum Decision {
    Nothing,
    Imported {
        observed: Box<ObservedUnit>,
        artifacts: Option<BTreeMap<String, ArtifactDocument>>,
    },
    Uncertain {
        observed: Box<ObservedUnit>,
        warning: Option<Finding>,
    },
    Warn(Finding),
}

/// Import the leftover runs' checkpoints into a run that resolved `resolved`.
/// Units whose execution key is not resolved yet are left for a later call
/// at the next wave's resolution, so call it until it imports nothing new.
pub fn import(inputs: &ImportInputs<'_>) -> Import {
    let mut import = Import::default();
    for (name, touched) in touches(inputs.leftovers) {
        let Some(newest) = touched.first() else {
            continue;
        };
        if inputs.handled.contains(&(newest.run.clone(), name.clone())) {
            continue;
        }
        let target = match (inputs.resolved.get(&name), inputs.adopted.get(&name)) {
            (Some(resolved), _) => Some((resolved, true)),
            (None, Some(adopted)) => Some((adopted, false)),
            (None, None) => None,
        };
        let decided = match (newest.touch, target) {
            // A unit without a desired document in this run has nothing to
            // record against.
            (_, None) => Some(Decision::Nothing),
            (Touch::Started { broken }, Some((target, _))) => Some(started(inputs, &name, newest, broken, target)),
            (Touch::Checkpointed(checkpoint), Some((target, keyed))) => {
                checkpointed(inputs, &name, newest.run, checkpoint, target, keyed)
            }
        };
        // The execution key is not resolved yet: a later call decides.
        let Some(decision) = decided else {
            continue;
        };
        for older in &touched[1..] {
            import.handled.insert((older.run.clone(), name.clone()));
            if let Touch::Checkpointed(checkpoint) = older.touch {
                if !not_newer(inputs.snapshot.get(&name), older.run, checkpoint) {
                    import.warnings.push(superseded_warning(
                        inputs.environment,
                        &name,
                        older.run,
                        &format!("the newer run {} touched the unit after it", newest.run),
                    ));
                }
            }
        }
        import.handled.insert((newest.run.clone(), name.clone()));
        match decision {
            Decision::Nothing => {}
            Decision::Imported { observed, artifacts } => {
                if let Some(artifacts) = artifacts {
                    import.artifacts.insert(name.clone(), artifacts);
                }
                import.observed.insert(name.clone(), *observed);
                import.imported.push(name);
            }
            Decision::Uncertain { observed, warning } => {
                import.warnings.extend(warning);
                import.observed.insert(name.clone(), *observed);
                import.recovered.push(name);
            }
            Decision::Warn(warning) => import.warnings.push(warning),
        }
    }
    import
}

/// The unit's observed document of `target`'s incarnation, as imported so far.
fn current<'o>(inputs: &'o ImportInputs<'_>, name: &UnitName, target: &DesiredUnit) -> Option<&'o ObservedUnit> {
    inputs
        .observed
        .get(name)
        .filter(|observed| observed.unit.uid == target.unit.uid)
}

/// The run an `uncertain` condition names, if the unit has one.
fn uncertain_run(current: Option<&ObservedUnit>) -> Option<&RunId> {
    match current.and_then(|current| current.condition.as_ref()) {
        Some(Condition::Uncertain { run, .. }) => Some(run),
        _ => None,
    }
}

/// Give the unit an `uncertain` condition naming the dead `run`.
fn mark_uncertain(
    inputs: &ImportInputs<'_>,
    target: &DesiredUnit,
    current: Option<&ObservedUnit>,
    run: &RunId,
    reason: String,
) -> Option<Box<ObservedUnit>> {
    let event = Event::Uncertain {
        since: inputs.now,
        run: run.clone(),
        reason,
    };
    match apply(&target.unit, current, event) {
        Ok(Applied::Changed(observed)) => Some(observed),
        Ok(Applied::Evidence) | Err(_) => None,
    }
}

/// A unit the newest leftover run started without a checkpoint.
fn started(
    inputs: &ImportInputs<'_>,
    name: &UnitName,
    touched: &Touched<'_>,
    broken: bool,
    target: &DesiredUnit,
) -> Decision {
    let current = current(inputs, name, target);
    if uncertain_run(current) == Some(touched.run) {
        return Decision::Nothing;
    }
    // A document that changed at or after the dead run's deadline came from
    // a later run, such as an earlier takeover whose run ref deletion failed.
    let changed_later = inputs
        .snapshot
        .get(name)
        .filter(|snapshot| snapshot.unit.uid == target.unit.uid)
        .and_then(changed_at)
        .is_some_and(|changed| changed >= touched.deadline);
    if changed_later {
        return Decision::Nothing;
    }
    match mark_uncertain(
        inputs,
        target,
        current,
        touched.run,
        started_reason(touched.run, broken),
    ) {
        Some(observed) => Decision::Uncertain {
            observed,
            warning: None,
        },
        None => Decision::Nothing,
    }
}

/// A unit the newest leftover run checkpointed; `None` while the unit's
/// execution key is not resolved.
fn checkpointed(
    inputs: &ImportInputs<'_>,
    name: &UnitName,
    run: &RunId,
    checkpoint: &Checkpoint,
    target: &DesiredUnit,
    keyed: bool,
) -> Option<Decision> {
    if not_newer(inputs.snapshot.get(name), run, checkpoint) {
        return Some(Decision::Nothing);
    }
    if keyed && target.execution_key.is_none() {
        return None;
    }
    let same_incarnation = target.unit.uid.is_some() && checkpoint.desired.unit.uid == target.unit.uid;
    let current = current(inputs, name, target);
    if same_incarnation && (!keyed || checkpoint.desired.execution_key == target.execution_key) {
        let event = Event::Imported {
            checkpointed: checkpoint.observed.clone(),
        };
        return Some(match apply(&target.unit, current, event) {
            Ok(Applied::Changed(observed)) => Decision::Imported {
                observed,
                artifacts: checkpointed_receipt(run, checkpoint).map(|_| checkpoint.artifacts.clone()),
            },
            Ok(Applied::Evidence) | Err(_) => Decision::Warn(late_warning(inputs.environment, name, run)),
        });
    }
    let why = if same_incarnation {
        "its execution key is not the unit's current one"
    } else {
        "it is of another incarnation of the unit"
    };
    // An uncertain unit keeps its condition: the run's own late result is
    // evidence, another run's result is superseded.
    match uncertain_run(current) {
        Some(named) if same_incarnation && named == run => {
            return Some(Decision::Warn(late_warning(inputs.environment, name, run)));
        }
        Some(_) if same_incarnation => {
            return Some(Decision::Warn(superseded_warning(inputs.environment, name, run, why)));
        }
        _ => {}
    }
    let warning = superseded_warning(inputs.environment, name, run, why);
    let receipt_key = current
        .and_then(|current| current.receipt.as_ref())
        .map(|receipt| &receipt.execution_key);
    let differs = receipt_key != checkpoint.desired.execution_key.as_ref();
    if same_incarnation && effects_real(run, checkpoint) && differs {
        let reason = format!("superseded result of run {run}: its effects are real");
        if let Some(observed) = mark_uncertain(inputs, target, current, run, reason) {
            return Some(Decision::Uncertain {
                observed,
                warning: Some(warning),
            });
        }
    }
    Some(Decision::Warn(warning))
}

/// What happens to the leftover and retired run refs after the takeover's
/// transition commit lands (or after a run that changed nothing releases).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Consumption {
    /// Leftover run refs (and signal refs) to delete: nothing on them is
    /// evidence for a unit that is `uncertain` at the end of the run.
    pub delete: Vec<RunId>,
    /// Leftover run refs to retire to evidence refs: a checkpoint on them is
    /// evidence for a unit still `uncertain` at the end of the run.
    pub retire: Vec<RunId>,
    /// Evidence refs to delete: no unit's `uncertain` condition needs them.
    pub delete_evidence: Vec<RunId>,
}

/// Whether `state` holds a checkpointed receipt, of the unit's current
/// incarnation, of a unit that is `uncertain` in `observed`: evidence
/// `nyl recover --accept` may adopt.
fn needed(state: &RunState, observed: &BTreeMap<UnitName, ObservedUnit>) -> bool {
    state.checkpoints.iter().any(|checkpoint| {
        checkpointed_receipt(&state.run.run, checkpoint).is_some()
            && observed.get(&checkpoint.desired.unit.name).is_some_and(|current| {
                current.unit.uid == checkpoint.desired.unit.uid
                    && matches!(current.condition, Some(Condition::Uncertain { .. }))
            })
    })
}

/// Decide [`Consumption`] from the observed documents as the run's
/// transition commit wrote them. Called once, after the waves.
pub fn consumed(leftovers: &Leftovers<'_>, observed: &BTreeMap<UnitName, ObservedUnit>) -> Consumption {
    let mut consumption = Consumption::default();
    for state in &leftovers.runs {
        if needed(state, observed) {
            consumption.retire.push(state.run.run.clone());
        } else {
            consumption.delete.push(state.run.run.clone());
        }
    }
    consumption.delete_evidence = leftovers
        .evidence
        .iter()
        .filter(|state| !needed(state, observed))
        .map(|state| state.run.run.clone())
        .collect();
    consumption
}

/// Narrow `consumption` to the refs whose version in `current`, the
/// coordination re-read right before consuming, is still the version
/// `leftovers` read. A leftover run whose lease was broken may still be
/// alive and checkpoint after the takeover read its ref; deleting or
/// retiring the ref then would lose a result the takeover never saw (or
/// bury a receipt that should be imported in an evidence ref). Such a ref,
/// and one already gone, is left alone: the next run reads it again, and
/// what was handled imports as a no-op. Contract:
/// [Runs and leases](../../../../design/orchestration-core.md#runs-and-leases), items 4 and 8.
pub fn unchanged(consumption: Consumption, leftovers: &Leftovers<'_>, current: &Coordination) -> Consumption {
    let same = |read: &[&RunState], now: &[RunState], run: &RunId| {
        let before = read
            .iter()
            .find(|state| &state.run.run == run)
            .map(|state| &state.version);
        let after = now
            .iter()
            .find(|state| &state.run.run == run)
            .map(|state| &state.version);
        before.is_some() && before == after
    };
    Consumption {
        delete: consumption
            .delete
            .into_iter()
            .filter(|run| same(&leftovers.runs, &current.runs, run))
            .collect(),
        retire: consumption
            .retire
            .into_iter()
            .filter(|run| same(&leftovers.runs, &current.runs, run))
            .collect(),
        delete_evidence: consumption
            .delete_evidence
            .into_iter()
            .filter(|run| same(&leftovers.evidence, &current.evidence, run))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::ports::HeldLease;
    use nyl_core::orchestration::state::{LeaseKind, Request, Run, RunKind, StateV1};
    use serde_json::json;

    use super::super::references::fixtures::{desired, env, name, observed, receipt, run, RUN_1, RUN_2};
    use super::*;

    const OWN: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b99";
    const OLDER: &str = "0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b80";

    fn at(time: &str) -> Timestamp {
        format!("2026-09-24T{time}:00Z").parse().unwrap()
    }

    fn unit(unit: &str, seed: &str) -> DesiredUnit {
        desired(unit, "Command", json!({}), seed)
    }

    fn success(unit: &str, seed: &str, by: &str, finished: &str) -> ObservedUnit {
        let mut receipt = receipt(seed, json!({}), Vec::new());
        receipt.run = run(by);
        receipt.started_at = at(finished);
        receipt.finished_at = at(finished);
        observed(unit, "Command", Some(receipt))
    }

    fn checkpoint(unit_name: &str, seed: &str, by: &str) -> Checkpoint {
        Checkpoint {
            desired: unit(unit_name, seed),
            observed: success(unit_name, seed, by, "10:00"),
            artifacts: BTreeMap::new(),
        }
    }

    fn run_state(id: &str, deadline: &str, units: &[&str], checkpoints: Vec<Checkpoint>) -> RunState {
        RunState {
            run: Run {
                api_version: StateV1,
                kind: RunKind,
                run: run(id),
                runner: "ci".to_owned(),
                operation: Operation::Reconcile,
                source_commit: None,
                read_desired: None,
                read_observed: None,
                deadline: at(deadline),
                units: units.iter().map(|unit| name(unit)).collect(),
                finished: false,
            },
            checkpoints,
            created: Vec::new(),
            version: "v1".to_owned(),
            foreign: false,
        }
    }

    fn lease(id: &str, operation: Operation, units: &[&str], broken: bool) -> Lease {
        Lease {
            api_version: StateV1,
            kind: LeaseKind,
            run: run(id),
            runner: "ci".to_owned(),
            operation,
            units: units.iter().map(|unit| name(unit)).collect(),
            deadline: at("11:00"),
            taken_at: at("09:00"),
            broken: broken.then(|| Request {
                by: "alice".to_owned(),
                at: at("10:30"),
                reason: None,
            }),
        }
    }

    /// A takeover's inputs: the coordination it read, the units as resolved,
    /// unresolved desired documents, and the snapshot's observed documents.
    struct World {
        own: RunId,
        replaced: Option<Lease>,
        coordination: Coordination,
        resolved: BTreeMap<UnitName, DesiredUnit>,
        adopted: BTreeMap<UnitName, DesiredUnit>,
        snapshot: BTreeMap<UnitName, ObservedUnit>,
        drivers: DriverSet,
    }

    impl World {
        fn new(runs: Vec<RunState>) -> Self {
            Self {
                own: run(OWN),
                replaced: None,
                coordination: Coordination {
                    lease: None,
                    runs,
                    evidence: Vec::new(),
                },
                resolved: BTreeMap::new(),
                adopted: BTreeMap::new(),
                snapshot: BTreeMap::new(),
                drivers: DriverSet::new(),
            }
        }

        fn resolve(&mut self, unit_name: &str, seed: &str) {
            self.resolved.insert(name(unit_name), unit(unit_name, seed));
        }

        fn leftovers(&self) -> Leftovers<'_> {
            leftovers(&self.own, self.replaced.as_ref(), &self.coordination)
        }

        /// `import` over `observed` (the snapshot when `None`) with `handled`.
        fn import_with(
            &self,
            snapshot: &BTreeMap<UnitName, ObservedUnit>,
            handled: &BTreeSet<(RunId, UnitName)>,
        ) -> Import {
            let leftovers = self.leftovers();
            import(&ImportInputs {
                leftovers: &leftovers,
                environment: &env(),
                now: at("12:00"),
                resolved: &self.resolved,
                adopted: &self.adopted,
                snapshot,
                observed: snapshot,
                handled,
                drivers: &self.drivers,
            })
        }

        fn import(&self) -> Import {
            self.import_with(&self.snapshot, &BTreeSet::new())
        }
    }

    fn codes(import: &Import) -> Vec<Code> {
        import.warnings.iter().map(|warning| warning.code).collect()
    }

    fn uncertain_by(import: &Import, unit: &str) -> Option<RunId> {
        match &import.observed.get(&name(unit))?.condition {
            Some(Condition::Uncertain { run, .. }) => Some(run.clone()),
            _ => None,
        }
    }

    /// Catches a checkpointed result with the resolved execution key being
    /// executed again instead of becoming the unit's receipt.
    #[test]
    fn test_import_takes_a_checkpoint_with_the_resolved_key_as_the_receipt() {
        let mut world = World::new(vec![run_state(
            RUN_2,
            "11:00",
            &[],
            vec![checkpoint("network", "v1", RUN_2)],
        )]);
        world.resolve("network", "v1");
        let import = world.import();
        assert_eq!(import.imported, [name("network")]);
        assert_eq!(
            import.observed[&name("network")],
            success("network", "v1", RUN_2, "10:00")
        );
        assert!(import.artifacts.contains_key(&name("network")));
        assert!(import.recovered.is_empty() && import.warnings.is_empty());
    }

    /// Catches a superseded result, whose effects are real, letting the unit
    /// look current or unchanged, whatever its recovery policy; and one
    /// whose key equals the current receipt's being marked needlessly.
    #[test]
    fn test_import_supersedes_another_key_and_marks_the_unit_uncertain_when_it_differs_from_the_receipt() {
        let mut world = World::new(vec![run_state(
            RUN_2,
            "11:00",
            &[],
            vec![checkpoint("database", "old", RUN_2), checkpoint("web", "old", RUN_2)],
        )]);
        world.resolve("database", "new");
        world.resolve("web", "new");
        // web's last receipt has the superseded key already.
        world
            .snapshot
            .insert(name("web"), success("web", "old", RUN_1, "08:00"));
        let import = world.import();
        assert_eq!(codes(&import), [Code::RunSuperseded, Code::RunSuperseded]);
        assert_eq!(uncertain_by(&import, "database"), Some(run(RUN_2)));
        assert!(!import.observed.contains_key(&name("web")));
        assert_eq!(import.recovered, [name("database")]);
        assert!(import.imported.is_empty());
    }

    /// Catches a unit a dead run was executing without a checkpoint keeping
    /// its last receipt as if nothing happened, or recording the importing
    /// run instead of the dead one; and a re-read leftover marking a unit
    /// that already names the dead run again.
    #[test]
    fn test_import_marks_units_started_without_a_checkpoint_uncertain_with_the_dead_run() {
        let mut world = World::new(vec![run_state(RUN_2, "11:00", &["database"], Vec::new())]);
        world.resolve("database", "v1");
        world
            .snapshot
            .insert(name("database"), success("database", "v0", RUN_1, "08:00"));
        let import = world.import();
        assert_eq!(uncertain_by(&import, "database"), Some(run(RUN_2)));
        assert_eq!(
            import.observed[&name("database")].receipt,
            world.snapshot[&name("database")].receipt,
            "a condition keeps the last receipt"
        );
        assert_eq!(import.recovered, [name("database")]);

        // The same leftover, read again after the condition reached state.
        world.snapshot.extend(import.observed);
        let again = world.import();
        assert_eq!(
            again,
            Import {
                handled: again.handled.clone(),
                ..Import::default()
            }
        );
    }

    /// Catches an import regressing state: a checkpoint older than what
    /// state already records must not replace it.
    #[test]
    fn test_import_skips_a_checkpoint_older_than_the_observed_document() {
        let mut world = World::new(vec![run_state(
            RUN_2,
            "11:00",
            &[],
            vec![checkpoint("network", "v1", RUN_2)],
        )]);
        world.resolve("network", "v1");
        world
            .snapshot
            .insert(name("network"), success("network", "v1", RUN_1, "10:30"));
        let import = world.import();
        assert!(import.observed.is_empty() && import.imported.is_empty() && import.warnings.is_empty());
        assert!(import.handled.contains(&(run(RUN_2), name("network"))));
    }

    /// Catches a late checkpoint clearing the uncertain condition it arrived
    /// after, and its run ref being deleted while it is the evidence
    /// `nyl recover --accept` needs.
    #[test]
    fn test_late_checkpoint_is_evidence_and_its_run_is_retired() {
        let mut late = checkpoint("database", "v1", RUN_2);
        late.observed = success("database", "v1", RUN_2, "12:30");
        let mut world = World::new(vec![run_state(RUN_2, "11:00", &[], vec![late])]);
        world.resolve("database", "v1");
        let mut uncertain = success("database", "v0", RUN_1, "08:00");
        uncertain.condition = Some(Condition::Uncertain {
            since: at("12:00"),
            run: run(RUN_2),
            reason: "run lost its lease while executing it".to_owned(),
        });
        world.snapshot.insert(name("database"), uncertain);
        let import = world.import();
        assert_eq!(codes(&import), [Code::RunLateCheckpoint]);
        assert!(import.warnings[0].fix.contains(&format!("--accept {RUN_2}")));
        assert!(import.observed.is_empty());

        let consumption = consumed(&world.leftovers(), &world.snapshot);
        assert_eq!(consumption.retire, [run(RUN_2)]);
        assert!(consumption.delete.is_empty());
        // Once the unit is no longer uncertain, the evidence goes.
        let mut retired = world.coordination.clone();
        retired.evidence = std::mem::take(&mut retired.runs);
        let cleared = BTreeMap::from([(name("database"), success("database", "v1", OWN, "13:00"))]);
        let consumption = consumed(&leftovers(&world.own, None, &retired), &cleared);
        assert_eq!(consumption.delete_evidence, [run(RUN_2)]);
    }

    /// Catches a takeover deleting the run ref of a run whose lease it broke
    /// but that is still alive, after that run wrote a checkpoint the
    /// takeover never read: the late result would be lost to import and to
    /// `nyl recover --accept`.
    #[test]
    fn test_consumption_leaves_run_refs_that_changed_since_they_were_read() {
        let world = World::new(vec![
            run_state(RUN_1, "10:30", &["network"], Vec::new()),
            run_state(RUN_2, "11:00", &["queue"], Vec::new()),
        ]);
        let leftovers = world.leftovers();
        let consumption = consumed(&leftovers, &BTreeMap::new());
        assert_eq!(consumption.delete, [run(RUN_1), run(RUN_2)]);

        let mut current = world.coordination.clone();
        // RUN_1 checkpointed after the read; RUN_2 is unchanged.
        current.runs[0].checkpoints.push(checkpoint("network", "v1", RUN_1));
        current.runs[0].version = "v2".to_owned();
        let narrowed = unchanged(consumption.clone(), &leftovers, &current);
        assert_eq!(narrowed.delete, [run(RUN_2)]);
        // A ref already gone is not deleted again.
        current.runs.clear();
        assert!(unchanged(consumption, &leftovers, &current).delete.is_empty());
    }

    /// Catches a crashed run's created unit getting a new uid (so its
    /// resources lose their state) when the new resolution no longer
    /// selects it, or its checkpoint not reaching state.
    #[test]
    fn test_adoption_reuses_the_created_uid_and_imports_a_deselected_unit() {
        let mut state = run_state(RUN_2, "11:00", &["cache"], vec![checkpoint("queue", "v1", RUN_2)]);
        state.created = vec![unit("cache", "v1"), unit("queue", "v1")];
        let mut world = World::new(vec![state]);
        let adopted = adoptions(&world.leftovers());
        assert_eq!(
            adopted.keys().cloned().collect::<Vec<_>>(),
            [name("cache"), name("queue")]
        );
        assert_eq!(adopted[&name("queue")].unit.uid, unit("queue", "v1").unit.uid);
        // Neither is selected any more: both import without a key comparison.
        world.adopted = adopted;
        let import = world.import();
        assert_eq!(import.imported, [name("queue")]);
        assert_eq!(import.recovered, [name("cache")]);
        assert_eq!(uncertain_by(&import, "cache"), Some(run(RUN_2)));
    }

    /// Catches a `--local` run taking over remote run refs, and a broken
    /// `verify` lease marking units uncertain although verify has no effects.
    #[test]
    fn test_foreign_runs_and_a_replaced_lease_without_effects_contribute_nothing() {
        let mut foreign = run_state(RUN_2, "11:00", &["database"], vec![checkpoint("network", "v1", RUN_2)]);
        foreign.foreign = true;
        let mut world = World::new(vec![foreign, run_state(OWN, "11:00", &["web"], Vec::new())]);
        world.replaced = Some(lease(RUN_1, Operation::Verify, &["database"], true));
        world.resolve("network", "v1");
        world.resolve("database", "v1");
        world.resolve("web", "v1");
        let leftovers = world.leftovers();
        assert!(leftovers.runs.is_empty() && leftovers.replaced.is_none());
        assert_eq!(world.import(), Import::default());

        // A broken reconcile lease's units are uncertain, naming the break.
        world.replaced = Some(lease(RUN_1, Operation::Reconcile, &["database"], true));
        let import = world.import();
        assert_eq!(uncertain_by(&import, "database"), Some(run(RUN_1)));
        assert!(matches!(
            &import.observed[&name("database")].condition,
            Some(Condition::Uncertain { reason, .. }) if reason.contains("broken")
        ));
    }

    /// Catches an older dead run's checkpoint winning over a newer run that
    /// touched the same unit after it.
    #[test]
    fn test_import_lets_the_newest_run_decide() {
        let mut world = World::new(vec![
            run_state(OLDER, "10:30", &[], vec![checkpoint("network", "v1", OLDER)]),
            run_state(RUN_2, "11:00", &["network"], Vec::new()),
        ]);
        world.resolve("network", "v1");
        let import = world.import();
        assert_eq!(uncertain_by(&import, "network"), Some(run(RUN_2)));
        assert_eq!(codes(&import), [Code::RunSuperseded]);
        assert!(import.imported.is_empty());
    }

    /// Catches a checkpoint of a unit whose key is not resolved yet being
    /// decided (superseded) before the key exists.
    #[test]
    fn test_import_waits_for_an_unresolved_key() {
        let mut world = World::new(vec![run_state(
            RUN_2,
            "11:00",
            &[],
            vec![checkpoint("database", "v1", RUN_2)],
        )]);
        world.resolve("database", "v1");
        world.resolved.get_mut(&name("database")).unwrap().execution_key = None;
        assert_eq!(world.import(), Import::default());
    }

    /// Catches the run importing its own run ref, or evidence refs being
    /// imported again as if they were leftover run refs.
    #[test]
    fn test_leftovers_keep_evidence_refs_apart_from_run_refs() {
        let mut coordination = Coordination {
            lease: Some(HeldLease {
                lease: lease(OWN, Operation::Reconcile, &[], false),
                version: "1".to_owned(),
            }),
            runs: vec![run_state(RUN_2, "11:00", &[], Vec::new())],
            evidence: vec![run_state(RUN_1, "09:00", &[], Vec::new())],
        };
        coordination.runs.push(run_state(OWN, "11:00", &[], Vec::new()));
        let own = run(OWN);
        let leftovers = leftovers(&own, None, &coordination);
        assert_eq!(leftovers.runs.len(), 1);
        assert_eq!(leftovers.evidence[0].run.run, run(RUN_1));
    }

    fn seed() -> impl proptest::strategy::Strategy<Value = &'static str> {
        proptest::sample::select(vec!["k1", "k2"])
    }

    /// One generated unit: its resolved key, the dead run's checkpointed key
    /// (if any), whether the dead run was executing it, and its stored
    /// receipt's key (if any).
    type GeneratedUnit = (&'static str, Option<&'static str>, bool, Option<&'static str>);

    fn generated_world(units: &[GeneratedUnit]) -> World {
        let names = ["alpha", "beta", "gamma", "delta"];
        let mut checkpoints = Vec::new();
        let mut started = Vec::new();
        let mut world = World::new(Vec::new());
        for (index, (resolved, checkpointed, running, stored)) in units.iter().enumerate() {
            let unit_name = names[index];
            world.resolve(unit_name, resolved);
            if let Some(seed) = checkpointed {
                checkpoints.push(checkpoint(unit_name, seed, RUN_2));
            } else if *running {
                started.push(unit_name);
            }
            if let Some(seed) = stored {
                world
                    .snapshot
                    .insert(name(unit_name), success(unit_name, seed, RUN_1, "08:00"));
            }
        }
        world.coordination.runs = vec![run_state(RUN_2, "11:00", &started, checkpoints)];
        world
    }

    proptest::proptest! {
        /// Catches import making a receipt of a result whose key the new
        /// resolution does not have, leaving a unit the dead run was
        /// executing without a condition, or changing anything when applied
        /// to its own output (a re-read leftover, with or without the
        /// handled pairs).
        #[test]
        fn test_import_properties(
            units in proptest::collection::vec(
                (seed(), proptest::option::of(seed()), proptest::bool::ANY, proptest::option::of(seed())),
                1..4,
            ),
        ) {
            let world = generated_world(&units);
            let import = world.import();
            let state = &world.coordination.runs[0];
            for unit in &import.imported {
                let checkpoint = state
                    .checkpoints
                    .iter()
                    .find(|checkpoint| &checkpoint.desired.unit.name == unit)
                    .expect("an imported unit was checkpointed");
                proptest::prop_assert_eq!(&checkpoint.desired.execution_key, &world.resolved[unit].execution_key);
            }
            for unit in &state.run.units {
                proptest::prop_assert!(
                    matches!(
                        &import.observed[unit].condition,
                        Some(Condition::Uncertain { run: dead, .. }) if dead == &state.run.run
                    ),
                    "unit {} the dead run was executing is not uncertain", unit
                );
            }

            let mut after = world.snapshot.clone();
            after.extend(import.observed.clone());
            let handled = import.handled.clone();
            for handled in [handled, BTreeSet::new()] {
                let again = world.import_with(&after, &handled);
                proptest::prop_assert!(again.observed.is_empty(), "{:?}", again.observed);
                proptest::prop_assert!(again.imported.is_empty() && again.recovered.is_empty());
            }
        }
    }
}
