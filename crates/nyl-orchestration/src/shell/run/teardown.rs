//! Execute leaving waves and publication teardown phases.
//!
//! Contract: [Deletion](../../../../../design/orchestration-core.md#deletion) and
//! [Kubernetes publication](../../../../../design/orchestration-core.md#kubernetes-publication-unit).
//! The pure planner owns ordering; this shell runs one batch, records outcomes,
//! and checks holds before planning again. Teardowns use stored desired documents
//! and recorded source revisions, without rendering.

#[allow(clippy::wildcard_imports)] // The run module's private helpers and imports.
use super::*;
use crate::rules::desired::{self as desired_rules, LifecycleOutcome, LifecycleWrite};
use crate::rules::lifecycle::{self, Event as LifecycleEvent, Next};
use crate::rules::teardown::{self as teardown_rules, Pending, TeardownInputs, TeardownOptions};
use crate::rules::transitions::{self, Applied, Event};
use nyl_core::orchestration::driver::{DriverError, DriverResult, WaitCompletion};
use nyl_core::orchestration::state::{
    DeletionIntent, DeletionReason, Hold, Request, TeardownProgress, TeardownWaiting,
};

fn hold_reason(reason: Option<&str>) -> String {
    reason
        .unwrap_or("teardown requested; automatic recreation is suspended")
        .to_owned()
}

/// The lifecycle state of one run: the stored desired documents as the run
/// leaves them, what it writes, and what it reports.
#[derive(Debug, Default)]
pub(super) struct Life {
    /// Every stored desired document after this run's lifecycle changes;
    /// removed documents are absent. Units of the ownership set appear as
    /// stored (or, when they return, as rendered).
    pub(super) desired: BTreeMap<UnitName, DesiredUnit>,
    /// The desired files the lifecycle changes write (`None` removes one).
    pub(super) writes: BTreeMap<UnitName, Option<DesiredUnit>>,
    /// Units whose observed file, observations, and attestations go.
    pub(super) cleared: BTreeSet<UnitName>,
    /// The results of leaving units, and of held units a hold signal or a
    /// teardown request reached.
    pub(super) results: BTreeMap<UnitName, UnitReport>,
    pub(super) details: Details,
    /// Units the run removes without a teardown: releases (kinds without
    /// teardown, `Retain` under `teardown --all`) and drops of tombstones.
    pub(super) releases: BTreeSet<UnitName>,
    /// The hold to apply after a unit's teardown or release ("tear down,
    /// then hold").
    pub(super) hold_after: BTreeMap<UnitName, Hold>,
    /// Units whose teardown wait goes on beyond this run.
    pub(super) waiting: BTreeSet<UnitName>,
    /// Units the run tore down, released, retained, or tried to.
    pub(super) attempted: BTreeSet<UnitName>,
    /// Cached preflight results, including successful checks and remnants.
    pub(super) readiness: BTreeMap<UnitName, crate::rules::teardown_readiness::Readiness>,
    /// The unit after which `--fail-fast` stopped the run.
    pub(super) stopped: Option<UnitName>,
    /// The hold signals the run acted on.
    pub(super) signals: super::Signals,
}

/// The teardown specifics of the transition commit's `details`.
#[derive(Debug, Default)]
pub(super) struct Details {
    pub(super) retained: Vec<UnitName>,
    pub(super) released: BTreeMap<UnitName, Option<String>>,
    pub(super) allow_dependents: BTreeMap<UnitName, Vec<UnitName>>,
    pub(super) external_dependents: BTreeMap<UnitName, Vec<String>>,
    pub(super) confirmed_removal: BTreeMap<UnitName, Value>,
    pub(super) incomplete: BTreeMap<UnitName, Vec<String>>,
    pub(super) teardown_requested: Vec<UnitName>,
    pub(super) held: Vec<UnitName>,
}

impl Details {
    /// The `details` entries, each only when not empty.
    pub(super) fn insert_into(&self, details: &mut BTreeMap<String, Value>) {
        if !self.retained.is_empty() {
            details.insert("retained".to_owned(), json!(self.retained));
        }
        if !self.released.is_empty() {
            details.insert("released".to_owned(), json!(self.released));
        }
        if !self.allow_dependents.is_empty() {
            details.insert("allowDependents".to_owned(), json!(self.allow_dependents));
        }
        if !self.external_dependents.is_empty() {
            details.insert("allowExternalDependents".to_owned(), json!(self.external_dependents));
        }
        if !self.confirmed_removal.is_empty() {
            details.insert("confirmedRemoval".to_owned(), json!(self.confirmed_removal));
        }
        if !self.incomplete.is_empty() {
            details.insert("incomplete".to_owned(), json!(self.incomplete));
        }
        if !self.teardown_requested.is_empty() {
            details.insert("teardownRequested".to_owned(), json!(self.teardown_requested));
        }
        if !self.held.is_empty() {
            details.insert("held".to_owned(), json!(self.held));
        }
    }
}

impl Life {
    /// The holds that later runs must apply when a teardown requested with
    /// a hold completes: `teardown --hold` (the deletion's `hold`), and a
    /// decommission (`--all`) of a unit still in the ownership set
    /// ([Instances](../../../../../design/orchestration-core.md#instances)).
    /// A `--hold` teardown that did not finish is held where it stopped and
    /// continues after `nyl resume`; a decommission that did not finish stays
    /// `deleting`. Whichever later run completes the teardown applies the
    /// hold recorded with the deletion's request, so the unit is not
    /// recreated until it is resumed again.
    pub(super) fn carry_decommission_holds(&mut self, ownership: &BTreeSet<UnitName>) {
        for (name, unit) in &self.desired {
            let Lifecycle::Deleting { deletion } = &unit.lifecycle else {
                continue;
            };
            let decommission = deletion.reason == DeletionReason::Decommission && ownership.contains(name);
            if !(deletion.hold || decommission) || self.hold_after.contains_key(name) {
                continue;
            }
            if let Some(requested) = &deletion.requested {
                self.hold_after.insert(
                    name.clone(),
                    Hold {
                        since: requested.at,
                        by: requested.by.clone(),
                        reason: Some(hold_reason(requested.reason.as_deref())),
                        pending: None,
                    },
                );
            }
        }
    }

    /// Record on a unit whose teardown was just requested with `--hold` that
    /// the hold applies once the teardown completes, also in a later run.
    fn mark_hold_after_teardown(&mut self, name: &UnitName) {
        let Some(unit) = self.desired.get_mut(name) else {
            return;
        };
        let (Lifecycle::Deleting { deletion }
        | Lifecycle::Held {
            deletion: Some(deletion),
            ..
        }) = &mut unit.lifecycle
        else {
            return;
        };
        deletion.hold = true;
        let updated = unit.clone();
        self.writes.insert(name.clone(), Some(updated));
    }

    pub(super) fn new(stored: &BTreeMap<UnitName, DesiredUnit>) -> Self {
        Self {
            desired: stored.clone(),
            ..Self::default()
        }
    }

    /// Move the unit's documents as `write` says.
    pub(super) fn apply(
        &mut self,
        name: &UnitName,
        write: LifecycleWrite,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) {
        let retained = matches!(
            write.desired.as_ref().map(|desired| &desired.lifecycle),
            Some(Lifecycle::Retained { .. })
        );
        if let Some(desired) = write.desired {
            self.desired.insert(name.clone(), desired.clone());
            self.writes.insert(name.clone(), Some(desired));
        } else {
            self.desired.remove(name);
            self.writes.insert(name.clone(), None);
        }
        if write.remove_observed {
            observed.remove(name);
            self.cleared.insert(name.clone());
            // A tombstone keeps the artifact files its receipt lists (Q12).
            if !retained {
                artifacts.remove(name);
            }
        }
    }

    fn result(&mut self, name: &UnitName, kind: &str, result: UnitResult) -> &mut UnitReport {
        let entry = self
            .results
            .entry(name.clone())
            .or_insert_with(|| UnitReport::new(kind.to_owned()));
        entry.result = Some(result);
        entry
    }
}

/// The hold of a held document's `hold` block.
fn held_pending(unit: &DesiredUnit) -> (Option<CommitId>, bool) {
    match &unit.lifecycle {
        Lifecycle::Held { hold, deletion } => (
            hold.pending.as_ref().map(|pending| pending.source_commit.clone()),
            deletion.is_some(),
        ),
        _ => (None, false),
    }
}

/// The report of a held unit and what its hold keeps from applying.
pub(super) fn held_report(
    environment: &EnvironmentName,
    name: &UnitName,
    unit: &DesiredUnit,
    recorded: bool,
) -> UnitReport {
    let (pending, teardown) = held_pending(unit);
    let mut entry = UnitReport::new(unit.unit.kind.clone());
    entry.result = Some(UnitResult::Held {
        pending: pending.clone(),
        teardown,
        recorded,
    });
    crate::rules::unit_state::lifecycle_details(environment, name, unit, None, &mut entry);
    entry
}

/// The `confirmedRemoval` entry of a completed wait.
fn completion_detail(completion: &WaitCompletion) -> Value {
    match completion {
        WaitCompletion::Confirmed { by, reason, from } => json!({"by": by, "reason": reason, "from": from}),
        WaitCompletion::Observed => json!("observed"),
        WaitCompletion::Delayed => json!("not-observed"),
        WaitCompletion::Incomplete => json!("incomplete"),
    }
}

/// The reason a pending unit's result names.
fn pending_reason(pending: &Pending) -> String {
    let list = |units: &[UnitName]| units.iter().map(UnitName::as_str).collect::<Vec<_>>().join(", ");
    match pending {
        Pending::ExternalDependents { units } => format!("other environments reference it: {}", units.join(", ")),
        Pending::NeedsAllowTeardown => "its teardown needs --allow-teardown".to_owned(),
        Pending::HasDependents { tombstones } => format!("retained tombstones cite it: {}", list(tombstones)),
        Pending::Dependents { units } => format!("waits for its deleting dependents: {}", list(units)),
        Pending::Waiting { units } => format!("waits for the teardown wait of {}", list(units)),
        Pending::Held => "held".to_owned(),
        Pending::Readiness { .. } => "its teardown readiness requirements are unmet".to_owned(),
        Pending::Uncertain => "it is uncertain; nyl recover resolves it first".to_owned(),
        Pending::FailedNonRetryable => "its teardown failed and is not retryable".to_owned(),
        Pending::NotNamed => "not named".to_owned(),
        Pending::AlreadyAttempted => "already attempted in this run".to_owned(),
        Pending::AwaitingApproval => "its teardown awaits an approval".to_owned(),
    }
}

/// One teardown call's inputs, prepared before its thread starts.
struct Job<'j> {
    unit: &'j DesiredUnit,
    observed: Option<&'j ObservedUnit>,
    commit: CommitId,
    slot: usize,
    directory: PathBuf,
    completion: Option<WaitCompletion>,
    approved_digest: Option<nyl_core::orchestration::ids::ChangeDigest>,
}

/// One teardown call's result.
struct Call {
    name: UnitName,
    outcome: DriverResult<Supported<Outcome>>,
    deadline: Timestamp,
    finished: Timestamp,
    progress: nyl_core::orchestration::progress::UnitProgress,
    killed: bool,
    /// The unit's files could not be checked out: nothing ran.
    unavailable: Option<String>,
}

/// What the leaving waves need beyond the run's state.
pub(super) struct LeaveInputs<'i> {
    /// The `nyl.toml` directory, worktree-rooted.
    pub(super) project_dir: &'i str,
    /// `--source` of a teardown run: the commit whose files every teardown uses.
    pub(super) source: Option<&'i CommitId>,
    pub(super) options: TeardownOptions<'i>,
    /// The leftover runs, which a hold signal's decision reads.
    pub(super) runs: &'i [nyl_core::orchestration::ports::RunState],
}

impl Reconcile<'_, '_, '_> {
    /// Run the leaving waves until nothing more can act in this run.
    pub(super) fn leave(
        &mut self,
        life: &mut Life,
        inputs: &LeaveInputs<'_>,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) -> Result<(), Ending> {
        let environment = self.environment().clone();
        loop {
            if self.session.request.cancellation.is_cancelled() {
                return Err(self.cancelled());
            }
            if life.stopped.is_some() {
                break;
            }
            // Holds sent to this run take effect before it starts a unit (Q28).
            self.apply_hold_signals(life, inputs.runs, observed, artifacts, None);
            self.preflight(life);
            let plan = self.leaving_plan(life, inputs, observed);
            // `--fail-fast` starts no teardown after the first approval wait.
            if let Some((name, _)) = plan.pending.iter().find(|(name, pending)| {
                matches!(pending, Pending::AwaitingApproval) && !life.results.contains_key(*name)
            }) {
                if self.session.request.fail_fast {
                    self.fail_fast(life, name);
                    break;
                }
            }
            if plan.wave.is_empty() {
                break;
            }
            // Run one batch, then read holds and plan again from its outcomes.
            let concurrency = self.session.request.concurrency.max(1);
            let batch: Vec<_> = plan.wave.into_iter().take(concurrency).collect();
            self.report.order_units(batch.iter().cloned());
            for name in &batch {
                if let Some(units) = plan.external_overrides.get(name) {
                    life.details.external_dependents.insert(name.clone(), units.clone());
                }
            }
            let (instant, teardowns): (Vec<_>, Vec<_>) = batch.into_iter().partition(|name| {
                life.releases.contains(name)
                    || matches!(&life.desired[name].lifecycle, Lifecycle::Deleting { deletion }
                        if deletion.intent == DeletionIntent::Retain)
            });
            for name in &instant {
                self.retire(life, name, observed, artifacts);
            }
            for name in &teardowns {
                if let Some(tombstones) = plan.overrides.get(name) {
                    life.details.allow_dependents.insert(name.clone(), tombstones.clone());
                }
            }
            self.tear_down(life, inputs, &teardowns, observed, artifacts)?;
        }
        // Every leaving unit this run did not finish reports why it waits.
        let plan = self.leaving_plan(life, inputs, observed);
        for (name, pending) in &plan.pending {
            if life.results.contains_key(name) || matches!(pending, Pending::NotNamed) {
                continue;
            }
            let Some(unit) = life.desired.get(name).cloned() else {
                continue;
            };
            if matches!(pending, Pending::Held) {
                let entry = held_report(&environment, name, &unit, false);
                life.results.insert(name.clone(), entry);
                continue;
            }
            let current = same_incarnation(&unit, observed.get(name));
            let reason = pending_reason(pending);
            let result = match (pending, current.and_then(|observed| observed.condition.as_ref())) {
                (Pending::Uncertain, Some(Condition::Uncertain { reason, .. })) => {
                    UnitResult::Uncertain { reason: reason.clone() }
                }
                (Pending::FailedNonRetryable, Some(Condition::Failed { category, .. })) => UnitResult::Failed {
                    category: *category,
                    retryable: false,
                },
                (Pending::AwaitingApproval, _) => UnitResult::AwaitingApproval,
                _ => match &unit.lifecycle {
                    Lifecycle::Deleting { deletion } if deletion.teardown == Some(TeardownProgress::InProgress) => {
                        UnitResult::TearingDown { reason }
                    }
                    _ => UnitResult::PendingTeardown { reason },
                },
            };
            let entry = life.result(name, &unit.unit.kind, result);
            entry.message = current
                .and_then(|observed| observed.condition.as_ref())
                .map(|condition| condition.message().to_owned());
            entry
                .warnings
                .extend(teardown_rules::finding(&environment, name, pending));
            if let Pending::Readiness { findings } = pending {
                entry.warnings.extend(findings.iter().cloned());
            }
        }
        Ok(())
    }

    /// The teardown plan over the run's current lifecycle state.
    fn leaving_plan(
        &self,
        life: &Life,
        inputs: &LeaveInputs<'_>,
        observed: &BTreeMap<UnitName, ObservedUnit>,
    ) -> teardown_rules::TeardownPlan {
        let external = self.session.external_dependents(&life.desired);
        let mut plan = teardown_rules::plan(&TeardownInputs {
            external_dependents: external.as_ref().unwrap_or(&BTreeMap::new()),
            environment: self.environment(),
            desired: &life.desired,
            observed,
            drivers: self.context().drivers,
            options: inputs.options,
            attempted: &life.attempted,
            waiting: &life.waiting,
            readiness: &life.readiness,
            drops: &life.releases,
        });
        if let Err(finding) = external {
            for name in plan.ready.drain(..) {
                plan.pending.insert(
                    name,
                    Pending::Readiness {
                        findings: vec![finding.clone()],
                    },
                );
            }
            plan.wave.clear();
        }
        plan
    }

    /// The readiness preflight of every deleting teardown unit, once.
    fn preflight(&self, life: &mut Life) {
        for (name, unit) in &life.desired {
            if life.readiness.contains_key(name)
                || !matches!(&unit.lifecycle, Lifecycle::Deleting { deletion }
                if deletion.intent == DeletionIntent::Teardown)
            {
                continue;
            }
            life.readiness.insert(
                name.clone(),
                readiness(self.context().drivers, self.environment(), name, unit).unwrap_or_default(),
            );
        }
    }

    /// Retain, release, or drop a unit that runs nothing.
    fn retire(
        &mut self,
        life: &mut Life,
        name: &UnitName,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) {
        life.attempted.insert(name.clone());
        let unit = life.desired[name].clone();
        let current = observed.get(name);
        let hold = life.hold_after.get(name).cloned();
        let tombstone_left = |unit: &DesiredUnit| match &unit.lifecycle {
            Lifecycle::Retained { tombstone } => tombstone.receipt.as_ref().and_then(|receipt| {
                let left: Vec<String> = receipt
                    .artifacts
                    .iter()
                    .map(|artifact| format!("{} {} ({})", artifact.kind, artifact.name, artifact.digest))
                    .collect();
                (!left.is_empty()).then(|| left.join(", "))
            }),
            _ => desired_rules::released_left(unit, current),
        };
        let left = tombstone_left(&unit);
        let release = LifecycleOutcome::Released { left: left.clone() };
        let (event, outcome) = match &unit.lifecycle {
            Lifecycle::Deleting { deletion }
                if !life.releases.contains(name)
                    && deletion.reason != DeletionReason::KindChange
                    && desired_rules::declares_retain(&unit) =>
            {
                let now = self.context().base.clock.now();
                (
                    LifecycleEvent::Retain {
                        tombstone: desired_rules::tombstone(&unit, current, now),
                    },
                    LifecycleOutcome::Retained,
                )
            }
            _ => (LifecycleEvent::Released { hold }, release),
        };
        let next = match lifecycle::transition(&unit.lifecycle, event) {
            Ok(next) => next,
            Err(refusal) => {
                self.report.warnings.push(Finding::new(
                    Code::TeardownPending,
                    format!("unit {name} cannot leave as planned: {refusal:?}"),
                    "report this as a bug in Nyl",
                ));
                return;
            }
        };
        let Some(write) = desired_rules::write(&unit, next, outcome.clone()) else {
            return;
        };
        life.apply(name, write, observed, artifacts);
        life.releases.remove(name);
        let environment = self.environment().clone();
        if outcome == LifecycleOutcome::Retained {
            life.details.retained.push(name.clone());
            life.result(name, &unit.unit.kind, UnitResult::Retained);
        } else {
            life.details.released.insert(name.clone(), left.clone());
            let entry = life.result(name, &unit.unit.kind, UnitResult::Released { left: left.clone() });
            entry.warnings.push(Finding::new(
                Code::UnitReleased,
                format!(
                    "unit {name} of {environment} left its resources in place{}",
                    left.as_ref().map(|left| format!(": {left}")).unwrap_or_default()
                ),
                "remove the resources by hand when nothing needs them any more",
            ));
        }
    }

    /// Tear down one chunk of units in parallel, then settle each outcome.
    #[allow(clippy::too_many_lines)] // One wave in order: worktrees, lease, driver calls, outcomes, waits.
    fn tear_down(
        &mut self,
        life: &mut Life,
        inputs: &LeaveInputs<'_>,
        chunk: &[UnitName],
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) -> Result<(), Ending> {
        let session = self.session;
        let context = session.context;
        let environment = self.environment().clone();
        let mut runnable = Vec::new();
        for name in chunk {
            life.attempted.insert(name.clone());
            let unit = life.desired[name].clone();
            let Some(commit) = inputs.source.cloned().or_else(|| unit.source_revision.clone()) else {
                self.source_missing(life, name, &unit, "it records no sourceRevision");
                continue;
            };
            runnable.push((name.clone(), unit, commit));
        }
        if runnable.is_empty() {
            return Ok(());
        }
        // Units waiting from an earlier run go straight to their wait.
        let (waiting, calls): (Vec<_>, Vec<_>) = runnable.into_iter().partition(|(name, unit, _)| {
            same_incarnation(unit, observed.get(name)).is_some_and(|observed| observed.waiting.is_some())
        });
        if !calls.is_empty() {
            let now = context.base.clock.now();
            let longest = longest_timeout(context.drivers, calls.iter().map(|(_, unit, _)| unit));
            let names: Vec<UnitName> = calls.iter().map(|(name, _, _)| name.clone()).collect();
            self.phase(names, lease::deadline(context.base, now, longest), &[])?;
            let results = {
                let jobs: Vec<Job<'_>> = calls
                    .iter()
                    .enumerate()
                    .map(|(slot, (name, unit, commit))| Job {
                        unit,
                        observed: same_incarnation(unit, observed.get(name)),
                        commit: commit.clone(),
                        slot,
                        directory: session.execution_directory(name),
                        completion: None,
                        approved_digest: session.request.approvals.units.get(name).cloned().flatten(),
                    })
                    .collect();
                run_jobs(context, &session.slots, &environment, inputs.project_dir, jobs)
            };
            for call in results {
                if call.killed {
                    return Err(self.killed(FaultPoint::AfterEffect));
                }
                let name = call.name.clone();
                let unit = life.desired[&name].clone();
                let commit = calls
                    .iter()
                    .find(|(called, _, _)| *called == name)
                    .map(|(_, _, commit)| commit.clone())
                    .expect("every call has a job");
                let waits = self.settle(life, &unit, call, observed, artifacts)?;
                if waits {
                    self.wait_then_finish(life, inputs, &name, &commit, observed, artifacts)?;
                }
            }
        }
        for (name, _, commit) in waiting {
            self.wait_then_finish(life, inputs, &name, &commit, observed, artifacts)?;
        }
        let _ = environment;
        Ok(())
    }

    /// A teardown that cannot check out its files fails before effects; the
    /// fix names `--source`.
    fn source_missing(&mut self, life: &mut Life, name: &UnitName, unit: &DesiredUnit, why: &str) {
        let environment = self.environment();
        let finding = Finding::new(
            Code::SourceUnavailable,
            format!("cannot tear down unit {name} of {environment}: {why}"),
            format!(
                "fetch the commit, or tear it down with another revision's files: `nyl teardown -e {environment} --unit \
                 {name} --source <revision>`"
            ),
        );
        self.report.errors.push(finding);
        let entry = life.result(
            name,
            &unit.unit.kind,
            UnitResult::Blocked {
                reason: format!("its teardown cannot start: {why}"),
            },
        );
        entry.message = Some(why.to_owned());
    }

    /// Record one teardown outcome; `true` when the unit now waits under its
    /// `teardownWait`.
    fn settle(
        &mut self,
        life: &mut Life,
        unit: &DesiredUnit,
        call: Call,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) -> Result<bool, Ending> {
        let progress = call.progress.clone();
        let finished = call.finished;
        let ending = self.settle_unit(life, unit, call, observed, artifacts);
        if let Some(entry) = life.results.get(&unit.unit.name) {
            if let Some(result) = &entry.result {
                crate::shell::progress::Progress::finish_event(
                    self.context(),
                    progress,
                    finished,
                    result,
                    entry.message.as_deref(),
                );
            }
        }
        ending
    }

    #[allow(clippy::too_many_lines)] // One outcome table: success, failure, uncertainty, wait, release.
    fn settle_unit(
        &mut self,
        life: &mut Life,
        unit: &DesiredUnit,
        call: Call,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) -> Result<bool, Ending> {
        let name = &call.name;
        let kind = unit.unit.kind.clone();
        let environment = self.environment().clone();
        if let Some(message) = &call.unavailable {
            self.source_missing(life, name, unit, message);
            return Ok(false);
        }
        let approved = self.session.request.approvals.records(
            &BTreeMap::from([(name.clone(), unit.execution_key.clone())]),
            &BTreeMap::from([(name.clone(), unit.clone())]),
        );
        if let Some(approval) = approved.get(name) {
            life.results
                .entry(name.clone())
                .or_insert_with(|| UnitReport::new(kind.clone()))
                .approval = Some(approval.clone());
        }
        let outcome = match call.outcome {
            Ok(Supported::Supported(outcome)) => outcome,
            Ok(Supported::Unsupported) => {
                // A kind that cannot tear down leaves its resources in place.
                life.releases.insert(name.clone());
                life.attempted.remove(name);
                self.retire(life, name, observed, artifacts);
                return Ok(false);
            }
            Err(error) if !error.effects_possible() => {
                let message = error.to_string();
                self.report.errors.push(Finding::new(
                    Code::DriverError,
                    format!("unit {name}: its teardown could not start: {message}"),
                    "fix the cause, such as a missing tool, then run the teardown again",
                ));
                let entry = life.result(
                    name,
                    &kind,
                    UnitResult::Blocked {
                        reason: format!("driver error: {message}"),
                    },
                );
                entry.message = Some(message);
                return Ok(false);
            }
            Err(error) => Outcome::Uncertain {
                reason: error.to_string(),
            },
        };
        let outcome = match outcome {
            // A teardown that ran past its deadline may have left anything.
            Outcome::Succeeded { .. } | Outcome::Failed { .. } | Outcome::RetryRequired { .. }
                if call.finished > call.deadline =>
            {
                Outcome::Uncertain {
                    reason: "the teardown passed the unit's timeout".to_owned(),
                }
            }
            outcome => outcome,
        };
        // The driver's teardown started: the teardown is in progress (Q8).
        let in_progress = match lifecycle::transition(&unit.lifecycle, LifecycleEvent::TeardownStarted) {
            Ok(Next::Lifecycle(lifecycle)) => DesiredUnit {
                lifecycle,
                ..unit.clone()
            },
            _ => unit.clone(),
        };
        let current = same_incarnation(unit, observed.get(name)).cloned();
        let condition = |event: Event| match transitions::apply_to(&in_progress, current.as_ref(), event) {
            Ok(Applied::Changed(document)) => Some(*document),
            Ok(Applied::Evidence) | Err(_) => None,
        };
        let since = call.finished;
        let run = self.run.clone();
        let (result, message, document, waits) =
            match outcome {
                Outcome::Succeeded { .. } => {
                    let hold = life.hold_after.get(name).cloned();
                    match lifecycle::transition(&in_progress.lifecycle, LifecycleEvent::TornDown { hold }) {
                        Ok(next) => {
                            if let Some(write) = desired_rules::write(&in_progress, next, LifecycleOutcome::Removed) {
                                life.apply(name, write, observed, artifacts);
                            }
                            if let Some(findings) = life.readiness.get(name).filter(|readiness| {
                                self.session.request.lifecycle.allow_incomplete && !readiness.ready()
                            }) {
                                let remains: Vec<String> = findings
                                    .blocking
                                    .iter()
                                    .map(|finding| finding.message.clone())
                                    .collect();
                                self.incomplete(life, name, remains);
                            }
                            life.result(name, &kind, UnitResult::Removed);
                        }
                        Err(refusal) => {
                            self.report.warnings.push(Finding::new(
                                Code::TeardownPending,
                                format!("unit {name}: its finished teardown was not recorded: {refusal:?}"),
                                "report this as a bug in Nyl",
                            ));
                        }
                    }
                    return Ok(false);
                }
                Outcome::AwaitingApproval { digest } => {
                    life.result(name, &kind, UnitResult::AwaitingApproval).change_digest = Some(digest);
                    self.fail_fast(life, name);
                    return Ok(false);
                }
                Outcome::RetryRequired {
                    category,
                    message,
                    context,
                } => (
                    UnitResult::Failed {
                        category,
                        retryable: false,
                    },
                    Some(message.clone()),
                    condition(Event::RetryRequired {
                        since,
                        run,
                        category,
                        message,
                        context,
                    }),
                    false,
                ),
                Outcome::Failed {
                    category,
                    retryable,
                    message,
                } => (
                    UnitResult::Failed { category, retryable },
                    Some(message.clone()),
                    condition(Event::Failed {
                        since,
                        run,
                        category,
                        retryable,
                        message,
                    }),
                    false,
                ),
                Outcome::Uncertain { reason } => (
                    UnitResult::Uncertain { reason: reason.clone() },
                    Some(reason.clone()),
                    condition(Event::Uncertain { since, run, reason }),
                    false,
                ),
                Outcome::Waiting {
                    phase,
                    reason,
                    until,
                    published,
                } => {
                    let waiting = TeardownWaiting {
                        phase,
                        reason: reason.clone(),
                        since,
                        until,
                        phase1_commit: published,
                        run,
                    };
                    (
                        UnitResult::TearingDown { reason: reason.clone() },
                        Some(reason),
                        condition(Event::Waiting { waiting }),
                        true,
                    )
                }
            };
        life.apply(
            name,
            LifecycleWrite {
                desired: Some(in_progress.clone()),
                remove_observed: false,
                outcome: LifecycleOutcome::PendingTeardown,
            },
            observed,
            artifacts,
        );
        if let Some(document) = document {
            observed.insert(name.clone(), document.clone());
            let checkpoint = Checkpoint {
                desired: in_progress,
                observed: document,
                artifacts: artifacts.get(name).cloned().unwrap_or_default(),
            };
            self.checkpointed = true;
            if let Err(error) = self.write_run(Some(&checkpoint)) {
                let finding = store_error(&environment, &error);
                self.report.errors.push(finding);
                return Err(Ending::KeepRun {
                    release: true,
                    unrecorded: vec![name.clone()],
                });
            }
        }
        let failed = !waits;
        let entry = life.result(name, &kind, result);
        entry.message = message;
        if failed {
            self.fail_fast(life, name);
        }
        Ok(waits)
    }

    /// `--fail-fast`: stop after `unit`.
    fn fail_fast(&self, life: &mut Life, unit: &UnitName) {
        if self.session.request.fail_fast && life.stopped.is_none() {
            life.stopped = Some(unit.clone());
        }
    }

    /// Record an incomplete teardown (Q55): what may remain, and
    /// `NYL-TEARDOWN-INCOMPLETE`.
    fn incomplete(&mut self, life: &mut Life, name: &UnitName, remains: Vec<String>) {
        let environment = self.environment().clone();
        let what = if remains.is_empty() {
            "its removal was neither observed nor confirmed".to_owned()
        } else {
            remains.join("; ")
        };
        self.report.warnings.push(Finding::new(
            Code::TeardownIncomplete,
            format!("the teardown of unit {name} of {environment} completed without a claim of removal: {what}"),
            "check what remains of the unit's resources and remove it by hand",
        ));
        life.details
            .incomplete
            .insert(name.clone(), if remains.is_empty() { vec![what] } else { remains });
    }

    /// Wait for a publication's teardown after phase 1, then run phase 2.
    #[allow(clippy::too_many_lines)] // One wait in order: confirmation or wait, its end, phase 2, its record.
    fn wait_then_finish(
        &mut self,
        life: &mut Life,
        inputs: &LeaveInputs<'_>,
        name: &UnitName,
        commit: &CommitId,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) -> Result<(), Ending> {
        let session = self.session;
        let context = session.context;
        let request = session.request;
        let unit = life.desired[name].clone();
        let Some(waiting) = same_incarnation(&unit, observed.get(name)).and_then(|observed| observed.waiting.clone())
        else {
            return Ok(());
        };
        let kind = unit.unit.kind.clone();
        let confirmed = request.lifecycle.confirm_removed
            && self
                .confirmable
                .get(name)
                .is_some_and(|recorded| recorded.phase1_commit == waiting.phase1_commit);
        let completion = if confirmed {
            // `--confirm-removed` of a wait recorded before this run, with
            // no other run waiting on it (the operator's direct confirmation).
            WaitCompletion::Confirmed {
                by: context.base.requested_by.clone(),
                reason: request.reason.clone(),
                from: context.base.runner.clone(),
            }
        } else {
            let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
            let strategy = crate::rules::teardown_wait::strategy(spec);
            let now = context.base.clock.now();
            let end = crate::rules::teardown_wait::ends(strategy, &waiting)
                .unwrap_or_else(|| unit_timeout(context.drivers, &unit).after(now))
                .max(now);
            let lease_deadline = context.base.settings.lease_grace.after(end);
            self.phase(vec![name.clone()], lease_deadline, &[])?;
            let request = super::wait::WaitRequest {
                location: self.location(),
                environment: self.environment(),
                run: &self.run,
                unit: &unit,
                waiting: &waiting,
                deadline: end,
                allow_incomplete: request.lifecycle.allow_incomplete,
                cancellation: &request.cancellation,
            };
            self.signalled = true;
            match super::wait::wait(context, &request) {
                super::wait::WaitEnd::Complete(completion) => completion,
                super::wait::WaitEnd::Stop { reason } => {
                    life.waiting.insert(name.clone());
                    let environment = self.environment().clone();
                    let entry = life.result(name, &kind, UnitResult::TearingDown { reason: reason.clone() });
                    entry.message = Some(reason.clone());
                    entry.warnings.push(Finding::new(
                        Code::TeardownWaiting,
                        format!("the teardown of unit {name} of {environment} waits: {reason}"),
                        format!(
                            "run nyl teardown or nyl reconcile again once the wait is over, or confirm the removal with \
                             `nyl teardown -e {environment} --unit {name} --confirm-removed`"
                        ),
                    ));
                    return Ok(());
                }
                super::wait::WaitEnd::Uncertain { reason } => {
                    let current = same_incarnation(&unit, observed.get(name)).cloned();
                    let event = Event::Uncertain {
                        since: context.base.clock.now(),
                        run: self.run.clone(),
                        reason: reason.clone(),
                    };
                    if let Ok(Applied::Changed(document)) = transitions::apply_to(&unit, current.as_ref(), event) {
                        observed.insert(name.clone(), *document);
                    }
                    let entry = life.result(name, &kind, UnitResult::Uncertain { reason: reason.clone() });
                    entry.message = Some(reason);
                    self.fail_fast(life, name);
                    return Ok(());
                }
                super::wait::WaitEnd::Held => {
                    self.apply_hold_signals(life, inputs.runs, observed, artifacts, None);
                    return Ok(());
                }
                super::wait::WaitEnd::Cancelled => return Err(self.cancelled()),
            }
        };
        self.apply_hold_signals(life, inputs.runs, observed, artifacts, None);
        if life
            .desired
            .get(name)
            .is_some_and(|unit| matches!(unit.lifecycle, Lifecycle::Held { .. }))
        {
            return Ok(());
        }
        life.details
            .confirmed_removal
            .insert(name.clone(), completion_detail(&completion));
        let incomplete = completion == WaitCompletion::Incomplete;
        let now = context.base.clock.now();
        self.phase(
            vec![name.clone()],
            lease::deadline(context.base, now, unit_timeout(context.drivers, &unit)),
            &[],
        )?;
        let environment = self.environment().clone();
        let results = run_jobs(
            context,
            &session.slots,
            &environment,
            inputs.project_dir,
            vec![Job {
                unit: &unit,
                observed: same_incarnation(&unit, observed.get(name)),
                commit: commit.clone(),
                slot: 0,
                directory: session.execution_directory(name),
                completion: Some(completion),
                approved_digest: session.request.approvals.units.get(name).cloned().flatten(),
            }],
        );
        for call in results {
            if call.killed {
                return Err(self.killed(FaultPoint::AfterEffect));
            }
            let finished = matches!(call.outcome, Ok(Supported::Supported(Outcome::Succeeded { .. })));
            if self.settle(life, &unit, call, observed, artifacts)? {
                // A second wait after phase 2 is not part of the protocol.
                life.waiting.insert(name.clone());
            }
            if finished && incomplete {
                let remains = life
                    .readiness
                    .get(name)
                    .map(|readiness| {
                        readiness
                            .remnants
                            .iter()
                            .map(|finding| finding.message.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                self.incomplete(life, name, remains);
            }
        }
        Ok(())
    }

    /// Mark the units `nyl teardown` names (or all of them), at the time
    /// the run took its lease.
    pub(super) fn request_teardowns(
        &mut self,
        life: &mut Life,
        target: &TeardownTarget,
        ownership: &BTreeSet<UnitName>,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) -> Result<(), Vec<Finding>> {
        let at = self.held.lease.taken_at;
        mark_teardowns(
            self.context(),
            self.session.request,
            life,
            target,
            ownership,
            at,
            observed,
            artifacts,
        )
    }

    /// "Tear down, then hold" for `--hold` teardowns that did not finish in
    /// this invocation: the deleting unit is held with its deletion block,
    /// so no later run recreates it; after `resume` the teardown continues.
    /// A decommission (`--all`) is not held here; see
    /// [`Life::carry_decommission_holds`].
    pub(super) fn hold_unfinished(
        &mut self,
        life: &mut Life,
        observed: &mut BTreeMap<UnitName, ObservedUnit>,
        artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
    ) {
        let environment = self.environment().clone();
        let holds: Vec<(UnitName, Hold)> = life
            .hold_after
            .iter()
            .map(|(name, hold)| (name.clone(), hold.clone()))
            .collect();
        for (name, hold) in holds {
            let Some(unit) = life.desired.get(&name).cloned() else {
                continue;
            };
            if !matches!(unit.lifecycle, Lifecycle::Deleting { .. } | Lifecycle::Active) || unit.unit.uid.is_none() {
                continue;
            }
            // A decommission stays deleting: its hold is applied by the run
            // that completes it (`carry_decommission_holds`).
            if matches!(&unit.lifecycle, Lifecycle::Deleting { deletion } if deletion.reason == DeletionReason::Decommission)
            {
                continue;
            }
            let Ok(mut next) = lifecycle::transition(&unit.lifecycle, LifecycleEvent::Hold { hold }) else {
                continue;
            };
            // The hold applies again once the teardown completes after
            // `nyl resume`, so the unit is not recreated.
            if let Next::Lifecycle(Lifecycle::Held {
                deletion: Some(deletion),
                ..
            }) = &mut next
            {
                deletion.hold = true;
            }
            if let Some(write) = desired_rules::write(&unit, next, LifecycleOutcome::PendingTeardown) {
                life.apply(&name, write, observed, artifacts);
            }
            let entry = life
                .results
                .entry(name.clone())
                .or_insert_with(|| UnitReport::new(unit.unit.kind.clone()));
            entry.warnings.push(Finding::new(
                Code::UnitHeld,
                format!(
                    "unit {name} of {environment} is held: its teardown did not finish in this run, and the hold keeps \
                     it from being recreated"
                ),
                format!("lift the hold with `nyl resume -e {environment} --unit {name}`; the teardown then continues"),
            ));
        }
    }
}

/// The readiness of a unit's teardown from its stored document: unmet
/// blocking requirements, and what it leaves behind; `None` when its kind
/// states no requirements.
pub(super) fn readiness(
    drivers: &DriverSet,
    environment: &EnvironmentName,
    name: &UnitName,
    unit: &DesiredUnit,
) -> Option<crate::rules::teardown_readiness::Readiness> {
    let driver = drivers.get(&unit.unit.api_version, &unit.unit.kind)?;
    match driver.teardown_settings(unit) {
        Ok(Supported::Supported(settings)) => {
            Some(crate::rules::teardown_readiness::evaluate(environment, name, &settings))
        }
        Ok(Supported::Unsupported) | Err(_) => None,
    }
}

/// Run teardown calls on a thread each, in their slots' worktrees.
fn run_jobs(
    context: &RunContext<'_>,
    slots: &Slots,
    environment: &EnvironmentName,
    project_dir: &str,
    jobs: Vec<Job<'_>>,
) -> Vec<Call> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|job| {
                let progress = crate::shell::progress::Progress::for_unit(
                    context,
                    environment,
                    job.unit,
                    Operation::Teardown,
                    None,
                )
                .into_event();
                scope.spawn(move || call(context, slots, environment, project_dir, job, &progress))
            })
            .collect();
        handles.into_iter().map(join).collect()
    })
}

/// One driver teardown call.
fn call(
    context: &RunContext<'_>,
    slots: &Slots,
    environment: &EnvironmentName,
    project_dir: &str,
    job: Job<'_>,
    progress: &nyl_core::orchestration::progress::UnitProgress,
) -> Call {
    let name = job.unit.unit.name.clone();
    let now = context.base.clock.now();
    let deadline = unit_timeout(context.drivers, job.unit).after(now);
    let finish = |outcome: DriverResult<Supported<Outcome>>, killed: bool| Call {
        name: name.clone(),
        outcome,
        deadline,
        finished: context.base.clock.now(),
        progress: progress.clone(),
        killed,
        unavailable: None,
    };
    let Some(driver) = context.drivers.get(&job.unit.unit.api_version, &job.unit.unit.kind) else {
        return finish(
            Err(DriverError::before_effects(format!(
                "no driver serves kind {}",
                job.unit.unit.kind
            ))),
            false,
        );
    };
    let temp_dir = job.directory.join("tmp");
    let worktree = std::fs::create_dir_all(&temp_dir)
        .map_err(|error| format!("cannot create {}: {error}", job.directory.display()))
        .and_then(|()| {
            slots.checkout_unit(
                context.sources,
                job.slot,
                &job.commit,
                driver,
                job.unit.resolved_spec.as_ref().unwrap_or(&job.unit.spec),
                project_dir,
            )
        });
    let (worktree, source_roots) = match worktree {
        Ok(roots) => roots,
        Err(message) => {
            let _ = std::fs::remove_dir_all(&job.directory);
            return Call {
                unavailable: Some(message.clone()),
                ..finish(Err(DriverError::before_effects(message)), false)
            };
        }
    };
    let (env, secrets) = match execution_credentials(context, driver, &job.unit.spec) {
        Ok(env) => env,
        Err(message) => {
            let _ = std::fs::remove_dir_all(&job.directory);
            let outcome = if matches!(
                job.observed.and_then(|observed| observed.condition.as_ref()),
                Some(Condition::Uncertain { .. })
            ) {
                Err(DriverError::before_effects(message))
            } else {
                Ok(Supported::Supported(Outcome::Failed {
                    category: nyl_core::orchestration::state::FailureCategory::Credentials,
                    retryable: true,
                    message,
                }))
            };
            return finish(outcome, false);
        }
    };
    let _ = context.base.faults.at(FaultPoint::Executing, Some(&name));
    let tools = crate::shell::tools::UnitRunner {
        tools: context.tools,
        unit: UnitAddress::new(environment.clone(), name.clone()),
    };
    let budget = crate::shell::tools::DeadlineRunner {
        tools: &tools,
        clock: context.base.clock,
        deadline,
    };
    let execution = ExecutionContext {
        secrets,
        interactive: false,
        artifacts: BTreeMap::new(),
        source_roots,
        unit: UnitAddress::new(environment.clone(), name.clone()),
        operation: DriverOperation::Teardown,
        project_dir: project_path(&worktree, project_dir),
        worktree,
        temp_dir,
        env,
        approved_digest: job.approved_digest,
        deadline,
        now,
        observed: job.observed,
        tools: &budget,
        commit_identity: context.commit_identity,
        cache_dir: context.cache_dir(),
        teardown_wait: job.completion,
    };
    let outcome = driver.teardown(&execution, job.unit);
    let killed = context.base.faults.at(FaultPoint::AfterEffect, Some(&name)) == FaultAction::Kill;
    let _ = std::fs::remove_dir_all(&job.directory);
    finish(outcome, killed)
}

/// Mark the units `nyl teardown` names (or all of them) for teardown,
/// release, or a recorded request on a held unit, requested at `at`.
/// Contract: [Deletion](../../../../../design/orchestration-core.md#deletion),
/// "Explicit teardown", and [Instances](../../../../../design/orchestration-core.md#instances)
/// (`--all` honours `Retain`, Q7; a held unit only records the request, Q26).
#[allow(clippy::too_many_arguments)] // The run's state the marks write, and when they were requested.
#[allow(clippy::too_many_lines)] // One table: held, released, tombstone, and teardown requests.
pub(super) fn mark_teardowns(
    context: &RunContext<'_>,
    request: &RunRequest,
    life: &mut Life,
    target: &TeardownTarget,
    ownership: &BTreeSet<UnitName>,
    at: Timestamp,
    observed: &mut BTreeMap<UnitName, ObservedUnit>,
    artifacts: &mut BTreeMap<UnitName, BTreeMap<String, ArtifactDocument>>,
) -> Result<(), Vec<Finding>> {
    let environment = request.environment.clone();
    let names: Vec<UnitName> = match target {
        TeardownTarget::Units(names) => {
            let unknown: Vec<Finding> = names
                .iter()
                .filter(|name| !life.desired.contains_key(*name))
                .map(|name| {
                    Finding::new(
                        Code::UnitUnknown,
                        format!("--unit {name} names no unit with state in {environment}"),
                        format!("list the environment's units with `nyl get units -e {environment}`"),
                    )
                })
                .collect();
            if !unknown.is_empty() {
                return Err(unknown);
            }
            names.iter().cloned().collect()
        }
        TeardownTarget::All => life.desired.keys().cloned().collect(),
    };
    let all = matches!(target, TeardownTarget::All);
    let requested = Request {
        by: context.base.requested_by.clone(),
        at,
        reason: request.reason.clone(),
    };
    let now = context.base.clock.now();
    for name in names {
        let unit = life.desired[&name].clone();
        let in_set = ownership.contains(&name);
        let reason = match (all, in_set) {
            (true, _) => DeletionReason::Decommission,
            (false, true) => DeletionReason::Replace,
            (false, false) => DeletionReason::Teardown,
        };
        let hold = (request.lifecycle.hold || (all && in_set)).then(|| Hold {
            since: now,
            by: context.base.requested_by.clone(),
            reason: Some(hold_reason(request.reason.as_deref())),
            pending: None,
        });
        let capable = !unit.is_held_marker()
            && context
                .drivers
                .get(&unit.unit.api_version, &unit.unit.kind)
                .is_some_and(|driver| driver.capabilities(&unit.spec).teardown);
        let retains = match &unit.lifecycle {
            Lifecycle::Retained { .. } => true,
            Lifecycle::Deleting { deletion } => deletion.intent == DeletionIntent::Retain,
            _ => desired_rules::deletion_intent(context.drivers, &unit) == DeletionIntent::Retain,
        };
        match &unit.lifecycle {
            Lifecycle::Held { .. } => {
                // A held file without an incarnation has nothing to tear down.
                if all && unit.unit.uid.is_none() {
                    continue;
                }
                let event = LifecycleEvent::TeardownRequested {
                    reason,
                    requested: requested.clone(),
                };
                if let Ok(next) = lifecycle::transition(&unit.lifecycle, event) {
                    if let Some(write) = desired_rules::write(&unit, next, LifecycleOutcome::PendingTeardown) {
                        life.apply(&name, write, observed, artifacts);
                    }
                }
                // A held unit never progresses (Q26): the request waits for
                // `nyl resume`, and `--hold` holds it again once torn down.
                if request.lifecycle.hold {
                    life.mark_hold_after_teardown(&name);
                }
                let entry = held_report(&environment, &name, &life.desired[&name], true);
                life.results.insert(name.clone(), entry);
                life.details.teardown_requested.push(name);
            }
            Lifecycle::Active if unit.unit.uid.is_none() => {}
            _ if !capable || (all && retains) => {
                life.releases.insert(name.clone());
                if let Some(hold) = hold {
                    life.hold_after.insert(name, hold);
                }
            }
            Lifecycle::Retained { tombstone } => {
                let tombstone = tombstone.clone();
                let event = LifecycleEvent::TeardownRequested {
                    reason,
                    requested: requested.clone(),
                };
                if let Ok(next) = lifecycle::transition(&unit.lifecycle, event) {
                    if let Some(write) = desired_rules::write(&unit, next, LifecycleOutcome::PendingTeardown) {
                        life.apply(&name, write, observed, artifacts);
                    }
                    // The tombstone's receipt is the observed file again.
                    if let Some(receipt) = tombstone.receipt {
                        let copy = DesiredUnit {
                            lifecycle: Lifecycle::Active,
                            ..unit.clone()
                        };
                        observed.insert(
                            name.clone(),
                            ObservedUnit {
                                api_version: nyl_core::orchestration::state::StateV1,
                                operator_modified: None,
                                retry: None,
                                kind: nyl_core::orchestration::state::ObservedUnitKind,
                                unit: unit.unit.clone(),
                                receipt: Some(receipt),
                                condition: None,
                                desired: Some(Box::new(copy)),
                                waiting: None,
                            },
                        );
                    }
                }
                if let Some(hold) = hold {
                    life.mark_hold_after_teardown(&name);
                    life.hold_after.insert(name, hold);
                }
            }
            Lifecycle::Active | Lifecycle::Deleting { .. } => {
                let event = LifecycleEvent::TeardownRequested {
                    reason,
                    requested: requested.clone(),
                };
                if let Ok(next) = lifecycle::transition(&unit.lifecycle, event) {
                    if let Some(write) = desired_rules::write(&unit, next, LifecycleOutcome::PendingTeardown) {
                        life.apply(&name, write, observed, artifacts);
                    }
                }
                if let Some(hold) = hold {
                    life.mark_hold_after_teardown(&name);
                    life.hold_after.insert(name, hold);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod hold_reason_tests {
    #[test]
    fn test_preserves_requested_reason() {
        assert_eq!(super::hold_reason(Some("maintenance")), "maintenance");
    }
}
