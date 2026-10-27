//! The leaving waves of a run: teardowns, retentions, releases, and drops,
//! in reverse dependency order, and `nyl teardown` as a run mode.
//!
//! Contract: [Deletion](../../../../../design/orchestration-core.md#deletion)
//! (omission, teardown, retained dependents, retain, explicit teardown,
//! holds, fencing), [Instances](../../../../../design/orchestration-core.md#instances)
//! (`teardown --all` honours `Retain`), [Kubernetes publication
//! unit](../../../../../design/orchestration-core.md#kubernetes-publication-unit)
//! (two-phase teardown, `teardownWait`, readiness), [Runs and
//! leases](../../../../../design/orchestration-core.md#runs-and-leases) item 2 (a
//! teardown wait's deadline), and [Recovery](../../../../../design/orchestration-core.md#recovery)
//! (the teardown recovery policy), as amended by the part 5 decisions (Q4,
//! Q6, Q7, Q8, Q10, Q11, Q26, Q50, Q55).
//!
//! - Every lifecycle change goes through [`lifecycle::transition`], and the
//!   documents move as [`desired_rules::write`] says; receipts and
//!   conditions follow [`transitions::apply_to`].
//! - Which unit acts next is [`teardown_rules::plan`]'s: consumers first,
//!   one reverse-dependency rule for teardowns, retentions, releases, and
//!   drops (Q6). A unit acts once no deleting unit that depends on it is
//!   left, and the run plans again after every wave.
//! - A teardown runs its driver in the unit's recorded `sourceRevision`
//!   (or `--source`), from the stored desired document; a teardown run
//!   renders nothing (Q4). Its outcome: success removes the unit's files
//!   (or keeps a held file without an incarnation, "tear down, then
//!   hold"); a failure or uncertain outcome leaves the teardown in progress
//!   with a condition (Q8); a two-phase teardown that waits records the wait
//!   and waits under its `teardownWait` ([`super::wait`]).
//! - A held unit never progresses: a teardown of it records the request
//!   only (Q26).

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
    /// Unmet blocking readiness requirements, and what a teardown leaves.
    pub(super) readiness: BTreeMap<UnitName, Vec<Finding>>,
    pub(super) remnants: BTreeMap<UnitName, Vec<String>>,
    pub(super) preflighted: BTreeSet<UnitName>,
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
    /// `teardown --all` holds every unit still in the ownership set once its
    /// teardown completes ([Instances](../../../../../design/orchestration-core.md#instances)).
    /// A decommission that did not finish in its invocation stays `deleting`
    /// (so `--confirm-removed` and running it again continue it); whichever
    /// later run completes the teardown applies the hold recorded by the
    /// deletion's request, so the unit is not recreated.
    pub(super) fn carry_decommission_holds(&mut self, ownership: &BTreeSet<UnitName>) {
        for (name, unit) in &self.desired {
            let Lifecycle::Deleting { deletion } = &unit.lifecycle else {
                continue;
            };
            if deletion.reason != DeletionReason::Decommission
                || !ownership.contains(name)
                || self.hold_after.contains_key(name)
            {
                continue;
            }
            if let Some(requested) = &deletion.requested {
                self.hold_after.insert(
                    name.clone(),
                    Hold {
                        since: requested.at,
                        by: requested.by.clone(),
                        reason: requested.reason.clone(),
                        pending: None,
                    },
                );
            }
        }
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

    /// The leaving units whose deleting dependents are all gone.
    fn unblocked(&self, ready: &[UnitName]) -> Vec<UnitName> {
        let dependencies = teardown_rules::dependencies(&self.desired);
        ready
            .iter()
            .filter(|name| {
                !dependencies.iter().any(|(consumer, producers)| {
                    consumer != *name && producers.contains(*name) && self.desired.get(consumer).is_some_and(leaving)
                })
            })
            .cloned()
            .collect()
    }
}

/// Whether a desired document is on its way out: deleting, or held with a
/// pending deletion.
pub(super) fn leaving(unit: &DesiredUnit) -> bool {
    matches!(
        unit.lifecycle,
        Lifecycle::Deleting { .. } | Lifecycle::Held { deletion: Some(_), .. }
    )
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

/// The report of a held unit: what its hold keeps from applying, and
/// `NYL-UNIT-HELD` when that is anything.
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
    entry.pending.clone_from(&pending);
    if pending.is_some() || teardown {
        let (what, then) = match (pending.as_ref(), teardown) {
            (Some(commit), true) => (
                format!("the changes of source commit {commit} and a pending teardown"),
                "to apply them; the next reconcile or teardown carries the teardown out",
            ),
            (Some(commit), false) => (format!("the changes of source commit {commit}"), "to apply them"),
            (None, _) => (
                "a pending teardown".to_owned(),
                "and the next reconcile or teardown carries it out",
            ),
        };
        entry.warnings.push(Finding::new(
            Code::UnitHeld,
            format!("unit {name} of {environment} is held; the hold keeps {what} from applying"),
            format!("lift the hold with `nyl resume -e {environment} --unit {name}` {then}"),
        ));
    }
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
}

/// One teardown call's result.
struct Call {
    name: UnitName,
    outcome: DriverResult<Supported<Outcome>>,
    deadline: Timestamp,
    finished: Timestamp,
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
        let context = self.context();
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
            self.preflight(life, inputs);
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
            let unblocked = life.unblocked(&plan.ready);
            if unblocked.is_empty() {
                break;
            }
            let (instant, teardowns): (Vec<UnitName>, Vec<UnitName>) = unblocked.into_iter().partition(|name| {
                life.releases.contains(name)
                    || matches!(&life.desired[name].lifecycle, Lifecycle::Deleting { deletion }
                        if deletion.intent == DeletionIntent::Retain)
            });
            for name in &instant {
                self.retire(life, name, observed, artifacts);
            }
            for (name, tombstones) in plan.overrides {
                if teardowns.contains(&name) {
                    life.details.allow_dependents.insert(name, tombstones);
                }
            }
            let concurrency = self.session.request.concurrency.max(1);
            for chunk in teardowns.chunks(concurrency) {
                if self.session.request.cancellation.is_cancelled() {
                    return Err(self.cancelled());
                }
                self.tear_down(life, inputs, chunk, observed, artifacts)?;
                if life.stopped.is_some() {
                    break;
                }
            }
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
        let _ = context;
        Ok(())
    }

    /// The teardown plan over the run's current lifecycle state.
    fn leaving_plan(
        &self,
        life: &Life,
        inputs: &LeaveInputs<'_>,
        observed: &BTreeMap<UnitName, ObservedUnit>,
    ) -> teardown_rules::TeardownPlan {
        teardown_rules::plan(&TeardownInputs {
            environment: self.environment(),
            desired: &life.desired,
            observed,
            drivers: self.context().drivers,
            options: inputs.options,
            attempted: &life.attempted,
            waiting: &life.waiting,
            readiness: &life.readiness,
            drops: &life.releases,
        })
    }

    /// The readiness preflight of every deleting teardown unit, once.
    fn preflight(&self, life: &mut Life, inputs: &LeaveInputs<'_>) {
        let _ = inputs;
        let units: Vec<(UnitName, DesiredUnit)> = life
            .desired
            .iter()
            .filter(|(name, unit)| {
                !life.preflighted.contains(*name)
                    && matches!(&unit.lifecycle, Lifecycle::Deleting { deletion }
                        if deletion.intent == DeletionIntent::Teardown)
            })
            .map(|(name, unit)| (name.clone(), unit.clone()))
            .collect();
        for (name, unit) in units {
            life.preflighted.insert(name.clone());
            let Some((blocking, remnants)) = readiness(self.context().drivers, self.environment(), &name, &unit) else {
                continue;
            };
            if !blocking.is_empty() {
                life.readiness.insert(name.clone(), blocking);
            }
            if !remnants.is_empty() {
                life.remnants.insert(name, remnants);
            }
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
            let longest = longest_timeout(calls.iter().map(|(_, unit, _)| unit));
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
    #[allow(clippy::too_many_lines)] // One outcome table: success, failure, uncertainty, wait, release.
    fn settle(
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
            Outcome::Succeeded { .. } | Outcome::Failed { .. } if call.finished > call.deadline => Outcome::Uncertain {
                reason: "the teardown passed the unit's timeout".to_owned(),
            },
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
        let (result, message, document, waits) = match outcome {
            Outcome::Succeeded { .. } => {
                let hold = life.hold_after.get(name).cloned();
                match lifecycle::transition(&in_progress.lifecycle, LifecycleEvent::TornDown { hold }) {
                    Ok(next) => {
                        if let Some(write) = desired_rules::write(&in_progress, next, LifecycleOutcome::Removed) {
                            life.apply(name, write, observed, artifacts);
                        }
                        if let Some(findings) = life
                            .readiness
                            .get(name)
                            .filter(|_| self.session.request.lifecycle.allow_incomplete)
                        {
                            let remains: Vec<String> = findings.iter().map(|finding| finding.message.clone()).collect();
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
            Outcome::AwaitingApproval { .. } => {
                life.result(name, &kind, UnitResult::AwaitingApproval);
                self.fail_fast(life, name);
                return Ok(false);
            }
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
                .unwrap_or_else(|| unit_timeout(&unit).after(now))
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
                super::wait::WaitEnd::Cancelled => return Err(self.cancelled()),
            }
        };
        life.details
            .confirmed_removal
            .insert(name.clone(), completion_detail(&completion));
        let incomplete = completion == WaitCompletion::Incomplete;
        let now = context.base.clock.now();
        self.phase(
            vec![name.clone()],
            lease::deadline(context.base, now, unit_timeout(&unit)),
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
                let remains = life.remnants.get(name).cloned().unwrap_or_default();
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
            let Ok(next) = lifecycle::transition(&unit.lifecycle, LifecycleEvent::Hold { hold }) else {
                continue;
            };
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
) -> Option<(Vec<Finding>, Vec<String>)> {
    let driver = drivers.get(&unit.unit.api_version, &unit.unit.kind)?;
    match driver.teardown_settings(unit) {
        Ok(Supported::Supported(settings)) => {
            let readiness = crate::rules::teardown_readiness::evaluate(environment, name, &settings);
            let remnants = readiness
                .remnants
                .iter()
                .map(|finding| finding.message.clone())
                .collect();
            Some((readiness.blocking, remnants))
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
            .map(|job| scope.spawn(move || call(context, slots, environment, project_dir, job)))
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
) -> Call {
    let name = job.unit.unit.name.clone();
    let now = context.base.clock.now();
    let deadline = unit_timeout(job.unit).after(now);
    let finish = |outcome: DriverResult<Supported<Outcome>>, killed: bool| Call {
        name: name.clone(),
        outcome,
        deadline,
        finished: context.base.clock.now(),
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
        .and_then(|()| slots.checkout(context.sources, job.slot, &job.commit));
    let worktree = match worktree {
        Ok(worktree) => worktree,
        Err(message) => {
            let _ = std::fs::remove_dir_all(&job.directory);
            return Call {
                unavailable: Some(message.clone()),
                ..finish(Err(DriverError::before_effects(message)), false)
            };
        }
    };
    let _ = context.base.faults.at(FaultPoint::Executing, Some(&name));
    let execution = ExecutionContext {
        unit: UnitAddress::new(environment.clone(), name.clone()),
        operation: DriverOperation::Teardown,
        project_dir: project_path(&worktree, project_dir),
        worktree,
        temp_dir,
        env: admitted_env(context.process_env, &job.unit.spec),
        approved_digest: None,
        deadline,
        now,
        observed: job.observed,
        tools: context.tools,
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
            reason: request.reason.clone(),
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
                                api_version: StateV1,
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
                    life.hold_after.insert(name, hold);
                }
            }
        }
    }
    Ok(())
}
