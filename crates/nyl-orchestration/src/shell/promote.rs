//! `nyl promote <path>`: the shell around the promotion decision core.
//!
//! Contract: [Promoting an environment's source](../../../../design/promotion.md#promoting-an-environments-source)
//! and [Exit categories](../../../../design/promotion.md#exit-categories). What
//! to promote is decided by [`crate::rules::promotion`]; this module performs
//! the effects around it:
//!
//! - **History.** It reads the source environment's recorded states from Git
//!   history ([`read_history`]): the desired and observed logs, whether each
//!   desired commit changed the environment's documents (as
//!   [`StateStore::state_commit`] defines it), the documents as of each state,
//!   and the attestations recorded for each state by walking the observed
//!   history from the state's commit to the tip.
//! - **Sequence.** The record's `sequence` is the highest one ever recorded
//!   into the target plus one, found in the target's desired history
//!   ([`next_sequence`]), never in the current snapshot: `state restore`
//!   removes newer records but not their commits.
//! - **Verification.** Each promoted artifact is checked with its producing
//!   driver's `verify_artifact` capability; the shell never names kinds.
//! - **Lease and write.** Under the target's lease it writes one desired
//!   commit holding the record and a summary. The source is only read, and
//!   its lease never taken. `--dry-run` takes no lease and writes nothing.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{ArtifactStatus, Supported, VerificationContext};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitAddress};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateSide, StateStore, StateTips, TransitionChanges};
use nyl_core::orchestration::promotion::{PromotionFrom, PromotionRecord, PromotionRequest};
use nyl_core::orchestration::state::{Operation, StateLocation};
use nyl_core::resources::promotion::PromotionPath;
use serde_json::{json, Value};

use super::lease;
use super::local::check_record;
use super::run::{execution_credentials, unit_timeout, RunContext, RunRequest};
use super::store_error;
use crate::report::{Outcome, PromoteGap, PromoteReport};
use crate::rules::freshness::receipt_is_current;
use crate::rules::promotion::{
    self, recorded_states, ArtifactCheck, Decision, DesiredEntry, IgnoreRequest, PromotionInput, RecordedState,
    Refusal, Request, Selection, StateAttestations,
};
use crate::transition::{trailer, Trailers, TransitionSummary};

/// How many commits of each state ref the history reads.
pub const HISTORY_COMMITS: usize = 500;
/// How many recorded states of the source a promotion considers, newest first.
pub const HISTORY_STATES: usize = 100;

/// What one `nyl promote` asks.
pub struct PromoteOptions {
    /// The validated path, from the source to the request's environment.
    pub path: PromotionPath,
    pub select: Selection,
    /// `--evidence published`.
    pub evidence_published: bool,
    /// `--ignore-attestation`.
    pub ignore: Vec<IgnoreRequest>,
    /// `--value`: the values to promote; empty promotes every value of the path.
    pub values: BTreeSet<String>,
    /// Whether promoted artifacts are verified: the path's `verifyArtifacts`
    /// unless `--no-verify-artifacts`.
    pub verify_artifacts: bool,
    pub dry_run: bool,
    /// The units the target environment also selects.
    pub target_units: BTreeSet<nyl_core::orchestration::ids::UnitName>,
    /// Whether the target's source is `fromPromotion` naming this path.
    pub promotes_source: bool,
    /// Who asks and why, recorded like an approval.
    pub requested: PromotionRequest,
}

/// `nyl promote`, for the environment `request` is for (the target).
pub fn promote(context: &RunContext<'_>, request: &RunRequest, options: &PromoteOptions) -> PromoteReport {
    super::run::off_runtime(|| promote_in(context, request, options))
}

fn source_of(path: &PromotionPath) -> Option<EnvironmentName> {
    path.spec
        .from
        .environment
        .as_deref()
        .and_then(|name| EnvironmentName::new(name).ok())
}

fn promote_in(context: &RunContext<'_>, request: &RunRequest, options: &PromoteOptions) -> PromoteReport {
    let target = request.environment.clone();
    let path_name = options.path.metadata.name.clone();
    let Some(source) = source_of(&options.path) else {
        return PromoteReport::new(&path_name, target.clone(), target, options.dry_run).refused(Finding::new(
            Code::PromoteUnsupported,
            format!("path {path_name} has no source environment"),
            "name an Environment in `spec.from.environment`",
        ));
    };
    let mut report = PromoteReport::new(&path_name, source.clone(), target.clone(), options.dry_run);
    let base = context.base;
    // A dry run takes no lease and writes nothing.
    let mut held = if options.dry_run {
        None
    } else {
        match lease::acquire(
            base,
            &request.location,
            &target,
            &base.ids.run_id(),
            Operation::Promote,
            crate::rules::lease::BrokenLease::Refuse,
            base.settings.resolution_timeout,
            request.wait,
            &request.cancellation,
        ) {
            Ok(held) => Some(held),
            Err(finding) => return report.refused(finding),
        }
    };
    let result = body(context, request, options, &source, held.as_mut(), &mut report);
    if let Some(held) = &held {
        if let Err(error) = lease::release(base, held, true) {
            report.warnings.push(store_error(&target, &error));
        }
    }
    match result {
        Ok(()) => report,
        Err(finding) => {
            let mut refused = report.refused(finding);
            refused.record = None;
            refused
        }
    }
}

#[allow(clippy::too_many_lines)] // One promotion in order: read, decide, verify, write.
fn body(
    context: &RunContext<'_>,
    request: &RunRequest,
    options: &PromoteOptions,
    source: &EnvironmentName,
    held: Option<&mut lease::HeldLease>,
    report: &mut PromoteReport,
) -> Result<(), Finding> {
    let base = context.base;
    let target = &request.environment;
    let location = &request.location;
    let store = base.store;
    let snapshot = store.snapshot(location).map_err(|error| store_error(target, &error))?;
    check_record(store, snapshot.record.as_ref(), target, location)?;
    let source_location = request.environments.get(source).ok_or_else(|| {
        request.environment_errors.get(source).cloned().unwrap_or_else(|| {
            Finding::new(
                Code::PromoteEnvironmentUnknown,
                format!(
                    "the source environment {source} of path {} is not declared",
                    options.path.metadata.name
                ),
                "declare the Environment, or correct the path's `spec.from`",
            )
        })
    })?;

    // The values to promote: a subset narrows the path, leaving the rest at their lineage.
    let mut path = options.path.clone();
    if !options.values.is_empty() {
        if options.promotes_source {
            return Err(Finding::new(
                Code::PromoteUnsupported,
                format!(
                    "path {} promotes the source of {target}: its source commit and every value move together, so --value cannot select a subset",
                    path.metadata.name
                ),
                "promote without --value",
            ));
        }
        if let Some(unknown) = options.values.iter().find(|name| !path.spec.values.contains_key(*name)) {
            return Err(Finding::new(
                Code::PromoteSelectorInvalid,
                format!("path {} declares no value {unknown:?}", path.metadata.name),
                format!(
                    "choose from: {}",
                    path.spec.values.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
        path.spec.values.retain(|name, _| options.values.contains(name));
    }
    let path_key = path.name().map_err(|error| {
        Finding::new(
            Code::PromoteSelectorInvalid,
            error.to_string(),
            "fix the PromotionPath's name",
        )
    })?;
    // The decision sees the path's record narrowed to the values promoted: an
    // equal subset leaves it `unchanged`.
    let mut target_records = snapshot.promotions.clone();
    if let Some(existing) = target_records.get_mut(&path_key) {
        if !options.values.is_empty() {
            existing.values.retain(|name, _| options.values.contains(name));
        }
    }

    let history = read_history(store, source_location, source)?;
    let history_sequence = next_sequence(store, location, &snapshot)?;
    let input = PromotionInput {
        path: &path,
        source,
        history: &history,
        target_units: &options.target_units,
        promotes_source: options.promotes_source,
        target_records: &target_records,
        sequence: history_sequence,
        now: base.clock.now(),
        request: &Request {
            select: options.select.clone(),
            evidence_published: options.evidence_published,
            ignore: options.ignore.clone(),
            requested: options.requested.clone(),
        },
    };
    let decision = promotion::decide(&input).map_err(|refusal| refusal_finding(&refusal, report))?;
    let promotion = match decision {
        Decision::Unchanged(record) => {
            report.record = Some(snapshot.promotions.get(&path_key).cloned().unwrap_or(*record));
            return Ok(());
        }
        Decision::Promote(promotion) => *promotion,
    };
    report.warnings.extend(promotion.warnings.clone());
    for ignore in &promotion.unused_ignores {
        report.warnings.push(Finding::new(
            Code::PromoteEvidenceOverride,
            format!(
                "--ignore-attestation {}/{} sets nothing aside: that attestation does not fail in the chosen state",
                ignore.scope, ignore.name
            ),
            "drop the option, or check `nyl get states` for the state's attestations",
        ));
    }
    let mut record = promotion.record;

    // Artifacts still exist where their receipts say they do.
    if options.verify_artifacts {
        verify(context, &history, &record, &promotion.artifacts, &mut report.warnings)?;
        record.artifacts_verified = unverified(&report.warnings).is_empty();
    }

    // Values the subset left alone stay at their lineage.
    if !options.values.is_empty() {
        if let Some(existing) = snapshot.promotions.get(&path_key) {
            for (name, value) in &existing.values {
                record.values.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    report.record = Some(record.clone());
    let Some(held) = held else {
        // A dry run: the record it would write.
        return Ok(());
    };

    let mut changes = TransitionChanges::default();
    changes.promotions.insert(path_key.clone(), Some(record.clone()));
    let mut summary = TransitionSummary {
        operation: Operation::Promote,
        environment: target.clone(),
        headline: format!("{path_key} #{}", record.sequence),
        units: BTreeMap::new(),
        recovered: Vec::new(),
        imported: Vec::new(),
        lease_broken: None,
        drift_repaired: Vec::new(),
        reapplied: None,
        details: BTreeMap::new(),
    };
    summary.details.insert("promotion".to_owned(), summary_of(&record));
    let trailers = Trailers {
        requested_by: Some(base.requested_by.clone()),
        reason: request.reason.clone(),
        source_commit: record.source_commit.clone(),
        ..super::trailers(base, held, &snapshot.tips)
    };
    let commits = super::commit_transition(base, location, held, &snapshot.tips, summary, changes, &trailers)
        .map_err(|error| store_error(target, &error))?;
    report.commits.extend(commits);
    report.outcome = Outcome::Done;
    Ok(())
}

/// The warnings that say an artifact could not be verified.
fn unverified(warnings: &[Finding]) -> Vec<&Finding> {
    warnings
        .iter()
        .filter(|finding| finding.code == Code::PromoteArtifactUnverified)
        .collect()
}

/// What the transition commit's summary says about the promotion.
fn summary_of(record: &PromotionRecord) -> Value {
    let from = match &record.from {
        PromotionFrom::Environment(from) => json!({
            "environment": from.environment,
            "run": from.run,
            "desiredCommit": from.desired_commit,
        }),
        PromotionFrom::Target(_) => Value::Null,
    };
    json!({
        "path": record.path,
        "sequence": record.sequence,
        "sourceCommit": record.source_commit,
        "from": from,
    })
}

/// The refusal as a finding, with the gaps it lists in the report.
fn refusal_finding(refusal: &Refusal, report: &mut PromoteReport) -> Finding {
    report.gaps = refusal
        .gaps
        .iter()
        .map(|gap| PromoteGap {
            scope: gap.scope.clone(),
            name: gap.name.clone(),
            message: gap.message.clone(),
        })
        .collect();
    refusal.finding.clone()
}

/// Check every artifact the promoted values read, with the driver that
/// produced it. A kind that cannot verify is a warning
/// (`NYL-PROMOTE-ARTIFACT-UNVERIFIED`), never silent; one whose artifact is gone
/// refuses (`NYL-PROMOTE-ARTIFACT-MISSING`, exit 1).
fn verify(
    context: &RunContext<'_>,
    history: &[RecordedState],
    record: &PromotionRecord,
    artifacts: &[ArtifactCheck],
    warnings: &mut Vec<Finding>,
) -> Result<(), Finding> {
    let PromotionFrom::Environment(from) = &record.from else {
        return Ok(());
    };
    for check in artifacts {
        let state_commit = record
            .values
            .get(&check.value)
            .and_then(|value| value.state.as_ref())
            .map_or(&from.desired_commit, |state| &state.desired_commit);
        let Some(state) = history.iter().find(|state| &state.desired_commit == state_commit) else {
            continue;
        };
        let name = &check.unit;
        let (Some(unit), Some(artifact)) = (
            state.snapshot.desired.get(name),
            state
                .snapshot
                .artifacts
                .get(name)
                .and_then(|set| set.get(&check.artifact)),
        ) else {
            continue;
        };
        let spec = unit.resolved_spec.as_ref().unwrap_or(&unit.spec);
        let Some(driver) = context.drivers.get(&unit.unit.api_version, &unit.unit.kind) else {
            warnings.push(unverified_warning(check, "no driver serves its kind"));
            continue;
        };
        if !driver.capabilities(spec).verify_artifact {
            warnings.push(unverified_warning(check, "its kind cannot verify artifacts"));
            continue;
        }
        let driver_error = |message: String| {
            Finding::new(
                Code::DriverError,
                format!("cannot verify artifact {} of unit {name}: {message}", check.artifact),
                "check the tool and registry access, or pass --no-verify-artifacts to record the promotion without the check",
            )
        };
        let (env, secrets) = execution_credentials(context, driver, spec).map_err(&driver_error)?;
        let temp_dir = context.workspace.join(format!("verify-{}-{}", check.value, name));
        std::fs::create_dir_all(&temp_dir).map_err(|error| driver_error(error.to_string()))?;
        let now = context.base.clock.now();
        let timeout = unit_timeout(context.drivers, unit);
        let ctx = VerificationContext {
            unit: UnitAddress::new(unit.unit.environment.clone(), name.clone()),
            env,
            secrets,
            temp_dir,
            now,
            deadline: timeout.after(now),
            tools: context.tools,
        };
        match driver.verify_artifact(&ctx, unit, artifact) {
            Ok(Supported::Supported(ArtifactStatus::Present)) => {}
            Ok(Supported::Supported(ArtifactStatus::Missing { reason })) => {
                return Err(Finding::new(
                    Code::PromoteArtifactMissing,
                    format!(
                        "artifact {} of unit {name}, which value {:?} reads, no longer exists: {reason}",
                        check.artifact, check.value
                    ),
                    "rebuild it in the source environment with `nyl reconcile --reapply`, promote another state, or pass --no-verify-artifacts if the registry cannot be reached from here",
                ));
            }
            Ok(Supported::Unsupported) => warnings.push(unverified_warning(check, "its kind cannot verify artifacts")),
            Err(error) => return Err(driver_error(error.to_string())),
        }
    }
    Ok(())
}

fn unverified_warning(check: &ArtifactCheck, why: &str) -> Finding {
    Finding::new(
        Code::PromoteArtifactUnverified,
        format!(
            "artifact {} of unit {}, which value {:?} reads, was not verified: {why}",
            check.artifact, check.unit, check.value
        ),
        "the record says artifactsVerified: false; check the artifact by hand",
    )
}

/// The recorded states of `environment` at `location`, newest first, with the
/// documents at each and the attestations recorded for it.
///
/// - A state is a transition commit pair (`recorded_states`): the desired
///   commits' log, with whether each changed the environment's documents,
///   which is what [`StateStore::state_commit`] answers for that commit, and
///   the observed log.
/// - The documents are read as of the state's commits. A run that wrote no
///   observed commit left the observed side as it read it (its
///   `Nyl-Read-Observed` trailer).
/// - Attestations are found by walking the observed history from the state's
///   observed commit to the tip: a later `nyl attest` names this state's
///   receipts or desired commit, and a later run drops them from the current
///   files, but history keeps them.
pub fn read_history(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
) -> Result<Vec<RecordedState>, Finding> {
    let error = |error| store_error(environment, &error);
    let desired_log = store
        .log(location, StateSide::Desired, HISTORY_COMMITS)
        .map_err(error)?;
    let observed_log = store
        .log(location, StateSide::Observed, HISTORY_COMMITS)
        .map_err(error)?;
    let mut entries = Vec::with_capacity(desired_log.len());
    for entry in &desired_log {
        let tips = StateTips {
            desired: Some(entry.commit.clone()),
            observed: None,
        };
        let changed = store
            .state_commit(location, &tips)
            .map_err(error)?
            .is_some_and(|commit| commit == entry.commit);
        entries.push(DesiredEntry {
            entry: entry.clone(),
            changed_desired: changed,
        });
    }
    let mut refs = recorded_states(environment, &entries, &observed_log);
    refs.truncate(HISTORY_STATES);

    // The attestations of every observed commit that can hold some, newest first.
    let writes_attestations = |message: &str| {
        trailer(message, "Nyl-Operation")
            .is_none_or(|operation| ["reconcile", "recover", "attest"].contains(&operation))
    };
    let mut attested: Vec<(CommitId, EnvironmentSnapshot)> = Vec::new();
    let oldest = refs
        .iter()
        .filter_map(|state| start_commit(state, &desired_log))
        .filter_map(|commit| observed_log.iter().position(|entry| entry.commit == commit))
        .max();
    if let Some(oldest) = oldest {
        for entry in observed_log[..=oldest]
            .iter()
            .filter(|entry| writes_attestations(&entry.message))
        {
            let tips = StateTips {
                desired: None,
                observed: Some(entry.commit.clone()),
            };
            attested.push((entry.commit.clone(), store.snapshot_at(location, &tips).map_err(error)?));
        }
    }

    let mut states = Vec::with_capacity(refs.len());
    for state in refs {
        let start = start_commit(&state, &desired_log);
        let tips = StateTips {
            desired: Some(state.desired_commit.clone()),
            observed: start.clone(),
        };
        let snapshot = store.snapshot_at(location, &tips).map_err(error)?;
        let from = start
            .as_ref()
            .and_then(|commit| observed_log.iter().position(|entry| &entry.commit == commit));
        let attestations = match from {
            Some(from) => {
                let newer: Vec<&EnvironmentSnapshot> = attested
                    .iter()
                    .filter(|(commit, _)| observed_log.iter().position(|entry| &entry.commit == commit) <= Some(from))
                    .map(|(_, snapshot)| snapshot)
                    .collect();
                attestations_of(&snapshot, &state.desired_commit, &newer)
            }
            None => StateAttestations::default(),
        };
        states.push(RecordedState {
            run: state.run,
            desired_commit: state.desired_commit,
            observed_commit: state.observed_commit,
            snapshot,
            attestations,
        });
    }
    Ok(states)
}

/// The observed commit a state's documents are read at: the run's own, or the
/// one the run read when it wrote none.
fn start_commit(
    state: &promotion::StateRef,
    desired_log: &[nyl_core::orchestration::ports::LogEntry],
) -> Option<CommitId> {
    if let Some(commit) = &state.observed_commit {
        return Some(commit.clone());
    }
    desired_log
        .iter()
        .find(|entry| trailer(&entry.message, "Nyl-Run-Id").is_some_and(|run| run.trim() == state.run.as_str()))
        .and_then(|entry| trailer(&entry.message, "Nyl-Read-Observed"))
        .and_then(|commit| CommitId::new(commit.trim()).ok())
}

/// The attestations recorded for `state`, from the observed snapshots at and
/// after its commit, newest first: per scope and name the newest entry that
/// names the state's receipt (and incarnation), or its desired commit.
fn attestations_of(
    state: &EnvironmentSnapshot,
    desired_commit: &CommitId,
    newer: &[&EnvironmentSnapshot],
) -> StateAttestations {
    let mut found = StateAttestations::default();
    for (unit, desired) in &state.desired {
        let observed = state.observed.get(unit);
        if !receipt_is_current(desired, observed) {
            continue;
        }
        let Some(receipt) = observed.and_then(|observed| observed.receipt.as_ref()) else {
            continue;
        };
        for snapshot in newer {
            for (name, attestation) in snapshot.attestations.get(unit).into_iter().flatten() {
                let names = attestation.execution_key == receipt.execution_key.as_str()
                    && attestation
                        .unit
                        .as_ref()
                        .is_none_or(|subject| Some(&subject.uid) == desired.unit.uid.as_ref());
                if names {
                    found
                        .units
                        .entry(unit.clone())
                        .or_default()
                        .entry(name.clone())
                        .or_insert_with(|| attestation.clone());
                }
            }
        }
    }
    for snapshot in newer {
        for (name, attestation) in &snapshot.environment_attestations {
            if attestation.execution_key == desired_commit.as_str() {
                found
                    .environment
                    .entry(name.clone())
                    .or_insert_with(|| attestation.clone());
            }
        }
    }
    found
}

/// The sequence the next record takes: one above the highest ever recorded
/// into the environment. The desired history holds every promotion's summary,
/// including those whose records `state restore` removed, so the sequence
/// never repeats; the current records count too, for state moved without its
/// history.
pub fn next_sequence(
    store: &dyn StateStore,
    location: &StateLocation,
    snapshot: &EnvironmentSnapshot,
) -> Result<u64, Finding> {
    let log = store
        .log(location, StateSide::Desired, HISTORY_COMMITS * 4)
        .map_err(|error| {
            Finding::new(
                Code::StateUnavailable,
                format!("cannot read the promotion history of the target: {error}"),
                "check access to the state repository",
            )
        })?;
    let from_history = log.iter().filter_map(|entry| recorded_sequence(&entry.message)).max();
    let from_records = snapshot.promotions.values().map(|record| record.sequence).max();
    Ok(from_history.max(from_records).map_or(1, |highest| highest + 1))
}

/// The sequence a `promote` transition commit's summary records.
pub fn recorded_sequence(message: &str) -> Option<u64> {
    if trailer(message, "Nyl-Operation") != Some("promote") {
        return None;
    }
    let line = message.lines().find_map(|line| line.strip_prefix("promotion: "))?;
    serde_json::from_str::<Value>(line).ok()?.get("sequence")?.as_u64()
}
