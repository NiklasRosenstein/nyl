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
//!   ([`next_sequence`]), never in the current snapshot alone: `state restore`
//!   removes newer records but not their commits. The current records, the
//!   sequence a run applied, and a `record: source` block count too.
//! - **Verification.** Each promoted artifact is checked with its producing
//!   driver's `verify_artifact` capability; the shell never names kinds.
//! - **Lease and write.** Under the target's lease it writes one desired
//!   commit holding the record and a summary. The source is only read, and
//!   its lease never taken. `--dry-run` takes no lease and writes nothing.
//! - **`record: source`.** A path that promotes the source of a target with
//!   `record: source` writes no state: the record becomes the target
//!   Environment's `source.fromPromotion.promoted` block, one commit on the
//!   branch the entry worktree has checked out with the same message a
//!   transition commit would carry, pushed fast-forward only through a
//!   [`PromotionRecordWriter`] (decision D3 of the M6 plan). The block in the
//!   entry worktree is the path's current record for `unchanged` (D2), and
//!   its sequence counts for the next one. A detached HEAD and a refused
//!   push write nothing ([`writer_finding`]).

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{ArtifactStatus, Supported, VerificationContext};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitAddress};
use nyl_core::orchestration::ports::{
    EnvironmentSnapshot, LogEntry, PromotionRecordWriter, RecordWriteError, StateHistory, StateStore, StateTips,
    TransitionChanges,
};
use nyl_core::orchestration::promotion::{PromotedState, PromotionFrom, PromotionRecord, PromotionRequest};
use nyl_core::orchestration::state::{Operation, StateLocation};
use nyl_core::resources::environment::PromotedSource;
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

/// How many recorded states of the source a promotion considers, newest first.
pub const HISTORY_STATES: usize = 100;
/// How many states `--revision` searches for its source commit, newest first.
pub const HISTORY_REVISION_STATES: usize = 1000;

/// What one `nyl promote` asks.
// Independent switches of the command line, as its options list them.
#[allow(clippy::struct_excessive_bools)]
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
    /// The units the target environment also selects, as its entry worktree
    /// renders them (not at each candidate's source commit).
    pub target_units: BTreeSet<nyl_core::orchestration::ids::UnitName>,
    /// `NYL-PROMOTE-REBUILT` warnings by value name
    /// ([`crate::rules::promotion_paths::rebuilt`]).
    pub rebuilt: Vec<(String, Finding)>,
    /// Whether the target's source is `fromPromotion` naming this path.
    pub promotes_source: bool,
    /// The target's `source.fromPromotion` as the entry worktree declares
    /// it: with `record: source`, its block is the path's current record.
    pub promoted: Option<PromotedSource>,
    /// Who asks and why, recorded like an approval.
    pub requested: PromotionRequest,
}

impl PromoteOptions {
    /// Whether the record goes into the target's source rather than its
    /// state: the path promotes the source of a target with `record: source`.
    pub fn records_in_source(&self) -> bool {
        self.promotes_source && crate::rules::sources::record_in_source(self.promoted.as_ref()).is_some()
    }
}

/// `nyl promote`, for the environment `request` is for (the target).
/// `writer` commits a `record: source` block ([`PromoteOptions::records_in_source`]).
pub fn promote(
    context: &RunContext<'_>,
    request: &RunRequest,
    options: &PromoteOptions,
    writer: Option<&dyn PromotionRecordWriter>,
) -> PromoteReport {
    super::run::off_runtime(|| promote_in(context, request, options, writer))
}

/// The path's source environment; `None` for a target source (M11).
pub fn source_of(path: &PromotionPath) -> Option<EnvironmentName> {
    path.spec
        .from
        .environment
        .as_deref()
        .and_then(|name| EnvironmentName::new(name).ok())
}

fn promote_in(
    context: &RunContext<'_>,
    request: &RunRequest,
    options: &PromoteOptions,
    writer: Option<&dyn PromotionRecordWriter>,
) -> PromoteReport {
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
    let result = body(context, request, options, (&source, writer), held.as_mut(), &mut report);
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
    (source, writer): (&EnvironmentName, Option<&dyn PromotionRecordWriter>),
    held: Option<&mut lease::HeldLease>,
    report: &mut PromoteReport,
) -> Result<(), Finding> {
    let base = context.base;
    let target = &request.environment;
    let location = &request.location;
    let store = base.store;
    // The order of `read_path`, with the invocation's own checks before the
    // source is read.
    let snapshot = read_target(store, target, location)?;
    let source_location = source_location(
        &request.environments,
        &request.environment_errors,
        source,
        &options.path.metadata.name,
    )?;

    // The values to promote: a subset narrows the path, leaving the rest at their lineage.
    let mut path = options.path.clone();
    if !options.values.is_empty() {
        if options.promotes_source {
            return Err(Finding::new(
                Code::PromoteSelectorInvalid,
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
    // The target's current records: in state, and with `record: source` the
    // block in the entry worktree. The decision sees the path's record
    // narrowed to the values promoted: an equal subset leaves it `unchanged`.
    let current = crate::rules::sources::current_records(
        target.as_str(),
        options.promoted.as_ref(),
        &snapshot.promotions,
        &crate::rules::sources::Applied::of(&snapshot),
    );
    let mut target_records = current.clone();
    if let Some(existing) = target_records.get_mut(&path_key) {
        if !options.values.is_empty() {
            existing.values.retain(|name, _| options.values.contains(name));
        }
    }

    let history = read_history(store, source_location, source, &options.select)?;
    let target_history = store.history(location).map_err(|error| store_error(target, &error))?;
    let block_sequence = crate::rules::sources::record_in_source(options.promoted.as_ref())
        .and_then(|promoted| promoted.promoted.as_ref())
        .map(|block| block.sequence);
    let history_sequence = next_sequence(&target_history, &snapshot, target, block_sequence);
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
            report.record = Some(current.get(&path_key).cloned().unwrap_or(*record));
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
    report.warnings.extend(
        options
            .rebuilt
            .iter()
            .filter(|(name, _)| record.values.contains_key(name))
            .map(|(_, finding)| finding.clone()),
    );

    // Artifacts still exist where their receipts say they do.
    if options.verify_artifacts {
        verify(context, &history, &record, &promotion.artifacts, &mut report.warnings)?;
        record.artifacts_verified = unverified(&report.warnings).is_empty();
    }

    // Values the subset left alone stay at their lineage.
    if !options.values.is_empty() {
        if let Some(existing) = current.get(&path_key) {
            // A carried value keeps the state it came from: the old record's `from`,
            // unless the value already names its own.
            let carried_from = match &existing.from {
                PromotionFrom::Environment(from) => Some(PromotedState {
                    run: from.run.clone(),
                    desired_commit: from.desired_commit.clone(),
                    observed_commit: from.observed_commit.clone(),
                }),
                PromotionFrom::Target(_) => None,
            };
            for (name, value) in &existing.values {
                record.values.entry(name.clone()).or_insert_with(|| {
                    let mut carried = value.clone();
                    if carried.state.is_none() {
                        carried.state.clone_from(&carried_from);
                    }
                    carried
                });
            }
        }
    }
    report.record = Some(record.clone());
    // Without a writer for a record kept in source, nothing can be written.
    let writer = if options.records_in_source() {
        Some(writer.ok_or_else(|| {
            writer_finding(
                &RecordWriteError::Failed("this invocation cannot write the project's source".to_owned()),
                target,
                &path_key,
            )
        })?)
    } else {
        None
    };
    let Some(held) = held else {
        // A dry run: the record it would write, which is `done` work not
        // done; a record kept in source is checked to be writable.
        if let Some(writer) = writer {
            writer
                .check(&record)
                .map_err(|error| writer_finding(&error, target, &path_key))?;
        }
        report.outcome = Outcome::Done;
        return Ok(());
    };

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
    if let Some(writer) = writer {
        // The record is a commit in source; the target's state is unchanged.
        let message = crate::transition::message(&summary, &trailers);
        let commit = writer
            .commit_record(&record, &message)
            .map_err(|error| writer_finding(&error, target, &path_key))?;
        report.commits.insert("source", commit);
        report.outcome = Outcome::Done;
        return Ok(());
    }
    let mut changes = TransitionChanges::default();
    changes.promotions.insert(path_key.clone(), Some(record.clone()));
    let commits = super::commit_transition(base, location, held, &snapshot.tips, summary, changes, &trailers)
        .map_err(|error| store_error(target, &error))?;
    report.commits.extend(commits);
    report.outcome = Outcome::Done;
    Ok(())
}

/// Why a `record: source` block was not committed, as the finding the
/// promotion is refused with. Nothing was written in any case.
pub fn writer_finding(
    error: &RecordWriteError,
    target: &EnvironmentName,
    path: &nyl_core::orchestration::ids::PromotionPathName,
) -> Finding {
    let again = format!("run `nyl promote {path}` again");
    let (code, fix) = match error {
        RecordWriteError::Detached => (
            Code::PromoteSourceUnwritable,
            format!("check out the branch the record belongs on, such as `git checkout main`, and {again}"),
        ),
        RecordWriteError::NoUpstream { branch } => (
            Code::PromoteSourceUnwritable,
            format!("the record is pushed to the branch's upstream: set one with `git branch --set-upstream-to=<remote>/{branch}`, and {again}"),
        ),
        RecordWriteError::Ahead { upstream, .. } => (
            Code::PromoteSourceUnwritable,
            format!("push those commits to {upstream} first, or drop them, so the promotion commit is the only one it publishes, and {again}"),
        ),
        RecordWriteError::Untracked { file } => (
            Code::PromoteSourceUnwritable,
            format!("commit {file} first, so the block lands in a file the branch carries, and {again}"),
        ),
        RecordWriteError::Dirty { file } => (
            Code::PromoteSourceUnwritable,
            format!("commit or discard the changes to {file}, so the promotion commit carries only its record, and {again}"),
        ),
        RecordWriteError::Unplaceable(_) => (
            Code::PromoteSourceUnwritable,
            format!("declare `spec.source.fromPromotion` on the Environment itself, in block style with one key per line, and {again}"),
        ),
        RecordWriteError::Undeclared(_) => (
            Code::PromoteSourceUnwritable,
            "keep the record in state (`record: state`), or declare the Environment in a file of the project".to_owned(),
        ),
        RecordWriteError::Rejected(_) => (
            Code::PromoteSourceRejected,
            format!("the branch moved since this checkout: update it (`git pull --ff-only`) and {again}; a promotion is never forced"),
        ),
        RecordWriteError::RemoteRejected(_) => (
            Code::PromoteSourceUnwritable,
            "the remote refused the push, such as for branch protection or a hook: allow this identity to push to the branch, or keep the record in state (`record: state`); promoting through a pull request (`changeGate: pullRequest`) comes in a later milestone".to_owned(),
        ),
        RecordWriteError::PushUnknown { .. } => (
            Code::PromoteSourceUnwritable,
            "the record may have been pushed: fetch the branch and check whether its tip is the promotion commit; if it is not, reset the local branch to its upstream and promote again".to_owned(),
        ),
        RecordWriteError::UndoFailed { commit, .. } => (
            Code::PromoteSourceUnwritable,
            format!("nothing was pushed, but the entry worktree still holds commit {commit}: drop it (`git reset --keep @{{upstream}}`) and {again}"),
        ),
        RecordWriteError::Failed(_) => (Code::PromoteSourceUnwritable, format!("fix the cause, then {again}")),
    };
    let written = match error {
        RecordWriteError::PushUnknown { .. } | RecordWriteError::UndoFailed { .. } => "",
        _ => ", so nothing was written",
    };
    Finding::new(
        code,
        format!("the promotion record of {target} cannot be committed to its source{written}: {error}"),
        fix,
    )
}

/// The PromotionRecords of `entry`'s environment as every entry point that
/// resolves it reads them (`plan`, `reconcile`, `build`, the direct
/// commands, and the multi-environment source graph):
/// [`crate::rules::sources::promotion_records`] over `snapshot`, and a
/// `record: source` block a run did not apply yet checked against the path
/// it names (one of `paths`) and the state of the path's source it names,
/// read from that environment's history by desired commit
/// ([`crate::rules::promotion::check_block`]). `location_of` finds an
/// environment's state for a path.
///
/// Readers that run nothing (`status`, `nyl get`) and the reading of a past
/// state's own records ([`states_of`]) do not come here: they show what is
/// recorded ([`crate::rules::sources::recorded_promotions`]).
pub fn environment_promotions(
    store: &dyn StateStore,
    location_of: &dyn Fn(&EnvironmentName, &str) -> Result<StateLocation, Finding>,
    paths: &[PromotionPath],
    entry: &nyl_core::resources::environment::Environment,
    snapshot: &EnvironmentSnapshot,
) -> Result<BTreeMap<nyl_core::orchestration::ids::PromotionPathName, PromotionRecord>, Finding> {
    use crate::rules::sources::{invalid_block, promotion_records, Applied};
    let environment = entry.metadata.name.as_str();
    let promoted = entry
        .spec
        .source
        .as_ref()
        .and_then(|source| source.from_promotion.as_ref());
    let read = promotion_records(environment, promoted, &snapshot.promotions, &Applied::of(snapshot))?;
    let Some(block) = &read.check else {
        return Ok(read.records);
    };
    let Some(path) = paths.iter().find(|path| path.metadata.name == block.path.as_str()) else {
        return Err(invalid_block(
            environment,
            format!("names path {}, which the project does not declare", block.path),
        ));
    };
    let (Some(source), PromotionFrom::Environment(from)) = (source_of(path), &block.from) else {
        return Err(invalid_block(
            environment,
            format!("names a source that path {} does not promote from", block.path),
        ));
    };
    if from.environment != source {
        return Err(invalid_block(
            environment,
            format!(
                "names a state of {}, and path {} promotes from {source}",
                from.environment, block.path
            ),
        ));
    }
    let location = location_of(&source, &path.metadata.name)?;
    let states = read_history(
        store,
        &location,
        &source,
        &Selection::StateRevision(from.desired_commit.clone()),
    )?;
    let Some(state) = states.first() else {
        return Err(Finding::new(
            Code::PromotionRecordInvalid,
            format!(
                "the promoted block of Environment {environment} names state {} of {source}, which {source}'s state history does not hold",
                from.desired_commit
            ),
            format!(
                "the state was removed (such as by `nyl state delete`) or recorded elsewhere: promote a state {source} records with `nyl promote {}`",
                block.path
            ),
        ));
    };
    crate::rules::promotion::check_block(block, path, state)?;
    Ok(read.records)
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

/// What a promotion along a path reads, for `nyl get promotion-candidates`
/// (and `nyl promote`, which reads the same two parts with its own checks
/// between them): the target's current state, whose records
/// the decision compares with, and the source's recorded states.
pub struct PathReading {
    /// The target environment's snapshot.
    pub snapshot: EnvironmentSnapshot,
    /// The source environment's recorded states, newest first.
    pub history: Vec<RecordedState>,
}

/// Read what a promotion decides on: the target's snapshot (its state must
/// exist) and the source's recorded states for `select`. Two syncs, one per
/// environment, whatever the length of the history.
pub fn read_path(
    store: &dyn StateStore,
    (target, target_location): (&EnvironmentName, &StateLocation),
    (source, source_location): (&EnvironmentName, &StateLocation),
    select: &Selection,
) -> Result<PathReading, Finding> {
    let snapshot = read_target(store, target, target_location)?;
    let history = read_history(store, source_location, source, select)?;
    Ok(PathReading { snapshot, history })
}

/// The target's snapshot; its state must exist.
pub fn read_target(
    store: &dyn StateStore,
    target: &EnvironmentName,
    location: &StateLocation,
) -> Result<EnvironmentSnapshot, Finding> {
    let snapshot = store.snapshot(location).map_err(|error| store_error(target, &error))?;
    check_record(store, snapshot.record.as_ref(), target, location)?;
    Ok(snapshot)
}

/// Where the path's source environment keeps its state, or why it cannot be
/// read: the finding its location resolved with, or an undeclared source.
pub fn source_location<'a>(
    environments: &'a BTreeMap<EnvironmentName, StateLocation>,
    errors: &BTreeMap<EnvironmentName, Finding>,
    source: &EnvironmentName,
    path: &str,
) -> Result<&'a StateLocation, Finding> {
    environments.get(source).ok_or_else(|| {
        errors.get(source).cloned().unwrap_or_else(|| {
            Finding::new(
                Code::PromoteEnvironmentUnknown,
                format!("the source environment {source} of path {path} is not declared"),
                "declare the Environment, or correct the path's `spec.from`",
            )
        })
    })
}

/// The recorded states of `environment` at `location`, newest first, with the
/// documents at each and the attestations recorded for it, and the history
/// they were read from.
///
/// - The history is read once, after the sync every read of state does
///   ([`StateStore::history`]): a stale local tracking ref never stands for
///   the remote, which could otherwise promote an older state under a higher
///   sequence. Everything below reads the objects that sync fetched.
/// - A state is a transition commit pair (`recorded_states`) over the whole
///   desired log, whose commits already say whether they changed the
///   environment's documents, and the observed log.
/// - The documents are read as of the state's commits. A run that wrote no
///   observed commit left the observed side as it read it (its
///   `Nyl-Read-Observed` trailer).
/// - Attestations are found by walking the observed history from the state's
///   observed commit to the tip: a later `nyl attest` names this state's
///   receipts or desired commit, and a later run drops them from the current
///   files, but history keeps them.
/// - Without an explicit choice the newest [`HISTORY_STATES`] states are read;
///   `--state-revision` reads just the named state, however old, and
///   `--revision` every state back to [`HISTORY_REVISION_STATES`].
pub fn read_history(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
    select: &Selection,
) -> Result<Vec<RecordedState>, Finding> {
    let history = store
        .history(location)
        .map_err(|error| store_error(environment, &error))?;
    states_of(store, location, environment, &history, select)
}

/// [`read_history`] over a history already read, without another sync.
pub fn states_of(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
    history: &StateHistory,
    select: &Selection,
) -> Result<Vec<RecordedState>, Finding> {
    let error = |error| store_error(environment, &error);
    let entries: Vec<DesiredEntry> = history
        .desired
        .iter()
        .map(|commit| DesiredEntry {
            entry: LogEntry {
                commit: commit.commit.clone(),
                message: commit.message.clone(),
            },
            changed_desired: commit.changed_desired,
        })
        .collect();
    let desired_log: Vec<LogEntry> = entries.iter().map(|entry| entry.entry.clone()).collect();
    let mut refs = recorded_states(environment, &entries, &history.observed);
    match select {
        Selection::Newest => refs.truncate(HISTORY_STATES),
        Selection::SourceRevision(_) => refs.truncate(HISTORY_REVISION_STATES),
        Selection::StateRevision(commit) => refs.retain(|state| state.desired_commit == *commit),
    }
    let observed_log = &history.observed;

    // The attestations of every observed commit that can hold some, newest
    // first, read in batches of objects the sync fetched.
    let writes_attestations = |message: &str| {
        trailer(message, "Nyl-Operation")
            .is_none_or(|operation| ["reconcile", "recover", "attest"].contains(&operation))
    };
    let position = |commit: &CommitId| observed_log.iter().position(|entry| &entry.commit == commit);
    let oldest = refs
        .iter()
        .filter_map(|state| start_commit(state, &desired_log))
        .filter_map(|commit| position(&commit))
        .max();
    let mut attested: Vec<(usize, EnvironmentSnapshot)> = Vec::new();
    if let Some(oldest) = oldest {
        let wanted: Vec<usize> = (0..=oldest)
            .filter(|index| writes_attestations(&observed_log[*index].message))
            .collect();
        for batch in wanted.chunks(32) {
            let tips: Vec<StateTips> = batch
                .iter()
                .map(|index| StateTips {
                    desired: None,
                    observed: Some(observed_log[*index].commit.clone()),
                })
                .collect();
            let snapshots = store.snapshots_at(location, &tips).map_err(error)?;
            attested.extend(batch.iter().copied().zip(snapshots.into_iter().map(attestations_only)));
        }
    }

    let starts: Vec<Option<CommitId>> = refs.iter().map(|state| start_commit(state, &desired_log)).collect();
    let tips: Vec<StateTips> = refs
        .iter()
        .zip(&starts)
        .map(|(state, start)| StateTips {
            desired: Some(state.desired_commit.clone()),
            observed: start.clone(),
        })
        .collect();
    let mut snapshots = Vec::with_capacity(tips.len());
    for batch in tips.chunks(16) {
        snapshots.extend(store.snapshots_at(location, batch).map_err(error)?);
    }
    let mut states = Vec::with_capacity(refs.len());
    for ((state, start), snapshot) in refs.into_iter().zip(starts).zip(snapshots) {
        let attestations = match start.as_ref().and_then(position) {
            Some(from) => {
                let newer: Vec<&EnvironmentSnapshot> = attested
                    .iter()
                    .filter(|(index, _)| *index <= from)
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

/// A snapshot reduced to the attestations it holds, so a long walk keeps
/// nothing else in memory.
fn attestations_only(snapshot: EnvironmentSnapshot) -> EnvironmentSnapshot {
    EnvironmentSnapshot {
        attestations: snapshot.attestations,
        environment_attestations: snapshot.environment_attestations,
        ..EnvironmentSnapshot::default()
    }
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
/// into the environment. The whole desired history holds every promotion's
/// summary, including those whose records `state restore` removed, so the
/// sequence never repeats. The current records, the sequence the environment
/// record says a run applied, and `others` (a `record: source` block's)
/// count too, for state moved without its history and for records kept in
/// source, which no promote commit in state carries. Environments that share
/// a desired ref (`spec.state.path`) share its history, so only promotions
/// into `environment` count.
pub fn next_sequence(
    history: &StateHistory,
    snapshot: &EnvironmentSnapshot,
    environment: &EnvironmentName,
    others: impl IntoIterator<Item = u64>,
) -> u64 {
    history
        .desired
        .iter()
        .filter_map(|commit| recorded_sequence(&commit.message, environment))
        .chain(snapshot.promotions.values().map(|record| record.sequence))
        .chain(
            snapshot
                .environment
                .as_ref()
                .and_then(|record| record.applied_promotion_sequence),
        )
        .chain(others)
        .max()
        .map_or(1, |highest| highest + 1)
}

/// The sequence a `promote` transition commit into `environment` records.
pub fn recorded_sequence(message: &str, environment: &EnvironmentName) -> Option<u64> {
    recorded_summary(message, environment)?.get("sequence")?.as_u64()
}

/// The `promotion` summary of a `promote` transition commit into
/// `environment`: its path, sequence, source commit, and source state, as
/// `promote` writes it. A commit whose `Nyl-Environment` names another
/// environment sharing the ref is not one.
pub fn recorded_summary(message: &str, environment: &EnvironmentName) -> Option<Value> {
    if trailer(message, "Nyl-Operation") != Some("promote") {
        return None;
    }
    if trailer(message, "Nyl-Environment").is_some_and(|name| name.trim() != environment.as_str()) {
        return None;
    }
    let line = message.lines().find_map(|line| line.strip_prefix("promotion: "))?;
    serde_json::from_str::<Value>(line).ok().filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(operation: &str, extra: &str) -> String {
        message_for(operation, extra, "prod")
    }

    fn message_for(operation: &str, extra: &str, environment: &str) -> String {
        format!(
            "nyl: {operation} {environment}\n\nunits: {{}}\n{extra}\n\nNyl-Operation: {operation}\nNyl-Run-Id: r\nNyl-Environment: {environment}\n"
        )
    }

    fn prod() -> EnvironmentName {
        EnvironmentName::new("prod").unwrap()
    }

    /// The sequence comes from the summary of `promote` commits only: a
    /// reconcile cannot carry one, and a message that merely resembles a
    /// summary does not count. Catches a sequence that restarts after a
    /// restore, or one that reads another operation's line.
    #[test]
    fn test_recorded_sequence_reads_promote_summaries_only() {
        let summary = "promotion: {\"path\":\"dev-to-prod\",\"sequence\":7}";
        assert_eq!(recorded_sequence(&message("promote", summary), &prod()), Some(7));
        assert_eq!(recorded_sequence(&message("reconcile", summary), &prod()), None);
        assert_eq!(
            recorded_sequence(&message("promote", "promotion: nonsense"), &prod()),
            None
        );
        assert_eq!(recorded_sequence(&message("promote", ""), &prod()), None);
    }

    /// Environments that share a desired ref (`spec.state.path`) share its
    /// history: a promotion into another of them never raises this
    /// environment's sequence. Catches a sequence that skips numbers, or
    /// one taken from a neighbour's history.
    #[test]
    fn test_next_sequence_counts_only_this_environments_promotions() {
        let commit = |message: String| nyl_core::orchestration::ports::DesiredCommit {
            commit: CommitId::new("a".repeat(40)).unwrap(),
            message,
            changed_desired: true,
        };
        let history = StateHistory {
            desired: vec![
                commit(message_for(
                    "promote",
                    "promotion: {\"path\":\"to-staging\",\"sequence\":9}",
                    "staging",
                )),
                commit(message(
                    "promote",
                    "promotion: {\"path\":\"dev-to-prod\",\"sequence\":2}",
                )),
            ],
            ..StateHistory::default()
        };
        assert_eq!(next_sequence(&history, &EnvironmentSnapshot::default(), &prod(), []), 3);
        let staging = EnvironmentName::new("staging").unwrap();
        assert_eq!(
            next_sequence(&history, &EnvironmentSnapshot::default(), &staging, []),
            10
        );
    }

    use std::collections::BTreeMap;

    use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
    use nyl_core::orchestration::state::{ArtifactUnit, Attestation, AttestationKind, AttestationResult, StateV1};
    use serde_json::Value;

    use crate::rules::fixtures::{desired, key, name, observed, receipt};

    fn attestation(
        unit: Option<&str>,
        uid: &str,
        execution_key: &str,
        result: AttestationResult,
        reason: &str,
    ) -> Attestation {
        Attestation {
            api_version: StateV1,
            kind: AttestationKind,
            name: "qa".to_owned(),
            unit: unit.map(|unit| ArtifactUnit {
                environment: EnvironmentName::new("dev").unwrap(),
                name: UnitName::new(unit).unwrap(),
                uid: nyl_core::orchestration::ids::Uid::new(uid).unwrap(),
            }),
            execution_key: execution_key.to_owned(),
            result,
            at: "2026-09-25T12:00:00Z".parse().unwrap(),
            source: Value::Null,
            reason: Some(reason.to_owned()),
            details: Value::Null,
        }
    }

    fn holding(unit: Option<(&str, Attestation)>, environment: Option<Attestation>) -> EnvironmentSnapshot {
        let mut snapshot = EnvironmentSnapshot::default();
        if let Some((unit, attestation)) = unit {
            snapshot
                .attestations
                .insert(name(unit), BTreeMap::from([("qa".to_owned(), attestation)]));
        }
        if let Some(attestation) = environment {
            snapshot.environment_attestations.insert("qa".to_owned(), attestation);
        }
        snapshot
    }

    /// The attestations of a state are the newest entry, among the observed
    /// snapshots at and after its commit, that names its receipt and
    /// incarnation (units) or its desired commit (environment): an entry for
    /// another receipt, another incarnation, or another state never counts,
    /// and an attestation recorded after a later run still does. Catches a
    /// walk that counts evidence of the wrong receipt or state, which would
    /// let a failing or unattested state pass.
    #[test]
    fn test_attestations_of_a_state_name_its_receipt_incarnation_and_commit() {
        let mut state = EnvironmentSnapshot::default();
        let svc = desired("svc", "Command", serde_json::json!({}), "svc");
        let uid = svc.unit.uid.clone().unwrap().to_string();
        state.desired.insert(name("svc"), svc);
        state.observed.insert(
            name("svc"),
            observed(
                "svc",
                "Command",
                Some(receipt("svc", serde_json::json!({}), Vec::new())),
            ),
        );
        let state_key = key("svc").to_string();
        let commit = CommitId::new("d".repeat(40)).unwrap();
        let other_uid = "00000000-0000-7000-8000-0000000000ff";
        let newest = holding(
            Some((
                "svc",
                attestation(Some("svc"), &uid, &state_key, AttestationResult::Fail, "newest"),
            )),
            Some(attestation(
                None,
                &uid,
                &"e".repeat(40),
                AttestationResult::Pass,
                "another state",
            )),
        );
        // Later runs replaced the receipt: entries for it do not count.
        let other_key = holding(
            Some((
                "svc",
                attestation(
                    Some("svc"),
                    &uid,
                    key("svc-2").as_ref(),
                    AttestationResult::Pass,
                    "other receipt",
                ),
            )),
            None,
        );
        let other_incarnation = holding(
            Some((
                "svc",
                attestation(Some("svc"), other_uid, &state_key, AttestationResult::Pass, "other uid"),
            )),
            None,
        );
        let recorded = holding(
            Some((
                "svc",
                attestation(
                    Some("svc"),
                    &uid,
                    &state_key,
                    AttestationResult::Pass,
                    "for this receipt",
                ),
            )),
            Some(attestation(
                None,
                &uid,
                commit.as_str(),
                AttestationResult::Pass,
                "for this commit",
            )),
        );
        let found = attestations_of(&state, &commit, &[&newest, &other_key, &other_incarnation, &recorded]);
        // The newest entry that names the receipt wins, failing or not.
        let unit = &found.units[&name("svc")]["qa"];
        assert_eq!(unit.reason.as_deref(), Some("newest"));
        assert_eq!(unit.result, AttestationResult::Fail);
        assert_eq!(found.environment["qa"].reason.as_deref(), Some("for this commit"));
        // Without the newest entry, the others are skipped and the recorded one counts.
        let found = attestations_of(&state, &commit, &[&other_key, &other_incarnation, &recorded]);
        assert_eq!(
            found.units[&name("svc")]["qa"].reason.as_deref(),
            Some("for this receipt")
        );
    }
}
