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
//! - **What runs.** Each unit's driver says which of its attestations are
//!   running-scoped and, per recorded attestation, what it shows each
//!   deployment runs ([`read_running`]), and maps a value's input selector
//!   to the deployment that consumes it ([`value_consumers`]); the rules
//!   judge these typed answers and never read an attestation's details.
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
//!   `record: source` writes the record as the target Environment's
//!   `source.fromPromotion.promoted` block, one commit on the branch the
//!   entry worktree has checked out with the same message a transition
//!   commit would carry, pushed fast-forward only through a
//!   [`PromotionRecordWriter`] (decision D3 of the M6 plan). Then, under the
//!   same lease, it writes a marker into the target's desired state: that
//!   message, and no record file ([`write_marker`]), which reserves the
//!   sequence and lists the promotion in the target's history. A marker that
//!   fails after the push leaves the promotion standing
//!   (`NYL-PROMOTE-MARKER-UNWRITTEN`), and the next `nyl promote`, finding
//!   the block `unchanged`, writes it. The block in the entry worktree is
//!   the path's current record for `unchanged` (D2), and its sequence counts
//!   for the next one. A detached HEAD and a refused push write nothing
//!   ([`writer_finding`]). A push that fails without a refusal may have
//!   reached the remote (`NYL-PROMOTE-SOURCE-UNCERTAIN`, exit 2): the commit
//!   stays on the local branch and no marker is written, since the record
//!   is not known to be published; the re-run that finds the pushed block
//!   `unchanged` writes the marker, through the same repair.
//! - **Target units.** A target whose source the path promotes selects its
//!   units as its definitions at each candidate's source commit say
//!   ([`target_units`]), not as the entry worktree has them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use nyl_core::orchestration::codes::{Code, Finding};
use nyl_core::orchestration::driver::{ArtifactStatus, DriverSet, Supported, VerificationContext};
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitAddress, UnitName};
use nyl_core::orchestration::ports::{
    BlockSignatureError, EnvironmentSnapshot, LogEntry, PromotionBlockVerifier, PromotionRecordWriter,
    RecordWriteError, SourceRenderer, SourceRepository, SourceResolver, StateHistory, StateStore, StateTips,
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
use crate::report::{Outcome, PromoteGap, PromoteMissing, PromoteMissingCommit, PromoteReport};
use crate::rules::freshness::receipt_is_current;
use crate::rules::promotion::supersede::{self, Item, RecordCommits, SupersedeEntry, SupersedeRequest};
use crate::rules::promotion::{
    self, recorded_states, ArtifactCheck, Decision, DesiredEntry, IgnoreRequest, PromotionInput, RecordedState,
    Refusal, Request, Selection, StateAttestations, TargetUnits, UnitRunning,
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
    /// The units the target environment selects as its entry worktree
    /// renders them: what the whole-source rule covers when the target does
    /// not run the promoted commit. A target whose source is promoted along
    /// the path selects its units as each candidate's source commit says
    /// ([`target_units`]).
    pub entry_units: BTreeSet<nyl_core::orchestration::ids::UnitName>,
    /// Whether the target's source is `fromPromotion` naming this path.
    pub promotes_source: bool,
    /// The target's `source.fromPromotion` as the entry worktree declares
    /// it: with `record: source`, its block is the path's current record.
    pub promoted: Option<PromotedSource>,
    /// Who asks and why, recorded like an approval.
    pub requested: PromotionRequest,
    /// `--supersede`: missing commits of other paths' records accepted as
    /// replaced.
    pub supersede: Vec<SupersedeRequest>,
    /// The names the project gives repositories (its GitRepositories), by
    /// which a refusal prints them and a `--supersede` entry qualifies them.
    pub repository_names: supersede::RepositoryNames,
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
    // Every `--supersede` entry has a reason before anything is read.
    supersede::require_reasons(&options.supersede, options.requested.reason.as_deref())?;
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
    // A block a run would refuse (invalid, or older than what ran) refuses
    // the promotion too.
    let current = crate::rules::sources::promotion_records(
        target.as_str(),
        options.promoted.as_ref(),
        &snapshot.promotions,
        &crate::rules::sources::Applied::of(&snapshot),
    )?
    .records;
    let mut target_records = current.clone();
    if let Some(existing) = target_records.get_mut(&path_key) {
        if !options.values.is_empty() {
            existing.values.retain(|name, _| options.values.contains(name));
        }
    }

    let mut history = read_history(store, source_location, source, &options.select)?;
    read_running(context.drivers, &mut history);
    let consumers = value_consumers(context.drivers, &history, &path);
    let target_history = store.history(location).map_err(|error| store_error(target, &error))?;
    let block_sequence = crate::rules::sources::record_in_source(options.promoted.as_ref())
        .and_then(|promoted| promoted.promoted.as_ref())
        .map(|block| block.sequence);
    let history_sequence = next_sequence(&target_history, &snapshot, target, block_sequence);
    let ask = Request {
        select: options.select.clone(),
        evidence_published: options.evidence_published,
        ignore: options.ignore.clone(),
        requested: options.requested.clone(),
    };
    let now = base.clock.now();
    let (decision, read_units) = settle_target_units(
        &TargetSources {
            sources: context.sources,
            renderer: context.renderer,
            workspace: context.workspace,
        },
        target,
        (options.promotes_source, &options.entry_units),
        (&history, &options.select),
        |units| {
            promotion::decide(&PromotionInput {
                path: &path,
                source,
                history: &history,
                target_units: units,
                promotes_source: options.promotes_source,
                consumers: &consumers,
                target_records: &target_records,
                sequence: history_sequence,
                now,
                request: &ask,
            })
        },
    );
    let decision = decision.map_err(|refusal| refusal_finding(&refusal, report))?;
    let chosen = match &decision {
        Decision::Unchanged(record) => &record.from,
        Decision::Promote(promotion) => &promotion.record.from,
    };
    if let PromotionFrom::Environment(from) = chosen {
        if let Some(finding) = passed_over(&read_units, &history, &options.select, &from.desired_commit) {
            return Err(finding);
        }
    }
    let mut held = held;
    let promotion = match decision {
        Decision::Unchanged(record) => {
            let record = current.get(&path_key).cloned().unwrap_or(*record);
            // A `record: source` block whose marker is missing from the
            // target's state, such as after a marker that failed to write,
            // gets it now: the retry the failure's fix names. Only a block
            // its branch's upstream carries is stamped.
            if let Some(held) = held.as_deref_mut() {
                if options.records_in_source() && !has_marker(&target_history, target, &record) {
                    let writer = writer.ok_or_else(|| {
                        writer_finding(
                            &RecordWriteError::Failed("this invocation cannot read the project's source".to_owned()),
                            target,
                            &path_key,
                        )
                    })?;
                    writer
                        .published()
                        .map_err(|error| writer_finding(&error, target, &path_key))?;
                    let commits = write_marker(context, request, held, &snapshot, &record)
                        .map_err(|error| store_error(target, &error))?;
                    report.commits.extend(commits);
                }
            }
            report.record = Some(record);
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
    // Nothing another path promoted since this path did is dropped.
    record.superseded = check_supersede(
        context,
        request,
        options,
        &SupersedeInput {
            source,
            history: &history,
            target_history: &target_history,
            current: &current,
            record: &record,
        },
        report,
    )?;

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

    let (summary, trailers) = transition_of(context, request, held, &snapshot, &record);
    if let Some(writer) = writer {
        // The record is a commit in source, pushed first; then the marker in
        // the target's state, under the same lease, reserves its sequence
        // and lists it in the target's history.
        let message = crate::transition::message(&summary, &trailers);
        let commit = writer
            .commit_record(&record, &message)
            .map_err(|error| writer_finding(&error, target, &path_key))?;
        report.commits.insert("source", commit.clone());
        report.outcome = Outcome::Done;
        match write_marker(context, request, held, &snapshot, &record) {
            Ok(commits) => report.commits.extend(commits),
            Err(error) => {
                // The promotion stands: its record is the pushed block. Only
                // the marker is missing, which a second `nyl promote` writes.
                let finding = marker_finding(&error, target, &path_key, &commit);
                report.exit = crate::run_report::ExitCategory::of_finding(&finding).code();
                report.errors.push(finding);
            }
        }
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

/// The summary and trailers of the `promote` transition that records
/// `record` into the target: the record's own commit in state, or the
/// message of a `record: source` commit and its marker.
fn transition_of(
    context: &RunContext<'_>,
    request: &RunRequest,
    held: &lease::HeldLease,
    snapshot: &EnvironmentSnapshot,
    record: &PromotionRecord,
) -> (TransitionSummary, Trailers) {
    let base = context.base;
    let mut summary = TransitionSummary {
        operation: Operation::Promote,
        environment: request.environment.clone(),
        headline: format!("{} #{}", record.path, record.sequence),
        units: BTreeMap::new(),
        recovered: Vec::new(),
        imported: Vec::new(),
        lease_broken: None,
        drift_repaired: Vec::new(),
        reapplied: None,
        details: BTreeMap::new(),
    };
    summary.details.insert("promotion".to_owned(), summary_of(record));
    let trailers = Trailers {
        requested_by: Some(base.requested_by.clone()),
        reason: request.reason.clone(),
        source_commit: record.source_commit.clone(),
        ..super::trailers(base, held, &snapshot.tips)
    };
    (summary, trailers)
}

/// The marker of a `record: source` promotion: one desired commit into the
/// target's state with the `promote` summary and trailers a `record: state`
/// promotion writes, and no record file
/// ([`TransitionChanges::commit_unchanged_desired`]). It changes no
/// document, so it is no state commit; [`next_sequence`] and the history
/// of `nyl get promotions` read its summary, so the sequence is reserved
/// across branches and the promotion is listed.
fn write_marker(
    context: &RunContext<'_>,
    request: &RunRequest,
    held: &mut lease::HeldLease,
    snapshot: &EnvironmentSnapshot,
    record: &PromotionRecord,
) -> Result<BTreeMap<&'static str, CommitId>, nyl_core::orchestration::ports::StateError> {
    let (summary, trailers) = transition_of(context, request, held, snapshot, record);
    let changes = TransitionChanges {
        commit_unchanged_desired: true,
        ..TransitionChanges::default()
    };
    super::commit_transition(
        context.base,
        &request.location,
        held,
        &snapshot.tips,
        summary,
        changes,
        &trailers,
    )
}

/// Whether the target's desired history holds the `promote` commit of
/// `record`: its record in state, or the marker of a `record: source` block.
fn has_marker(history: &StateHistory, target: &EnvironmentName, record: &PromotionRecord) -> bool {
    history.desired.iter().any(|commit| {
        recorded_summary(&commit.message, target).is_some_and(|summary| {
            summary.get("path").and_then(Value::as_str) == Some(record.path.as_str())
                && summary.get("sequence").and_then(Value::as_u64) == Some(record.sequence)
        })
    })
}

/// Why the marker of a pushed `record: source` commit was not written
/// (`NYL-PROMOTE-MARKER-UNWRITTEN`, exit 1). The promotion stands; running
/// `nyl promote` again finds the block unchanged and writes the marker.
fn marker_finding(
    error: &nyl_core::orchestration::ports::StateError,
    target: &EnvironmentName,
    path: &nyl_core::orchestration::ids::PromotionPathName,
    commit: &CommitId,
) -> Finding {
    Finding::new(
        Code::PromoteMarkerUnwritten,
        format!(
            "the promotion record of {target} was committed to its source as {commit} and pushed, but its marker in {target}'s state was not written: {error}; until it is, the sequence is not reserved in state and `nyl get promotions` does not list the promotion"
        ),
        format!("the promotion stands; once the state is reachable, update the checkout to the pushed commit and run `nyl promote {path}` again: while the block still names the state it would promote, it writes only the marker, under your Nyl-Requested-By and Nyl-Reason; if a newer state qualifies by then, it promotes that instead, with a marker of its own"),
    )
}

/// What the supersede check of one promotion reads.
struct SupersedeInput<'a> {
    source: &'a EnvironmentName,
    /// The source's recorded states the record's values came from.
    history: &'a [RecordedState],
    /// The target's history, whose `promote` commits hold earlier records.
    target_history: &'a StateHistory,
    /// The target's current records.
    current: &'a BTreeMap<nyl_core::orchestration::ids::PromotionPathName, PromotionRecord>,
    /// The record the promotion would write.
    record: &'a PromotionRecord,
}

/// The source fields of a desired unit, as its driver declares them.
fn source_fields_of<'a>(
    context: &'a RunContext<'_>,
) -> impl Fn(&nyl_core::orchestration::state::DesiredUnit) -> Vec<String> + 'a {
    |unit| {
        context
            .drivers
            .get(&unit.unit.api_version, &unit.unit.kind)
            .map(|driver| driver.source_fields(unit.resolved_spec.as_ref().unwrap_or(&unit.spec)))
            .unwrap_or_default()
    }
}

/// The supersede check ([`supersede`]): the commits of every record another
/// path wrote into the target since this path last promoted each item must
/// be contained in what this promotion writes, unless `--supersede` accepts
/// them as replaced. Returns the superseded commits the record lists; a
/// refusal lists what is missing in the report.
///
/// - Records come from the target's current records and from the desired
///   state at each `promote` commit of its history ([`records_to_compare`]),
///   so a record `state restore` removed still counts. A `record: source`
///   block's earlier blocks are commits in source, not in state: only the
///   current block counts.
/// - A record's value has the source commits of its producing unit in the
///   source state the record names ([`record_commits`]); a state the source
///   environment's history no longer holds, or an environment no longer
///   declared, leaves it unchecked (`NYL-PROMOTE-UNCHECKED`).
/// - A shallow entry clone is refused (`NYL-PROMOTE-SHALLOW-CLONE`): its
///   histories end early. Commits are fetched into the entry clone without
///   reading their trees; a record's commit that cannot be fetched is
///   unchecked unless `--supersede` names it.
fn check_supersede(
    context: &RunContext<'_>,
    request: &RunRequest,
    options: &PromoteOptions,
    input: &SupersedeInput<'_>,
    report: &mut PromoteReport,
) -> Result<Vec<nyl_core::orchestration::promotion::SupersededCommit>, Finding> {
    let target = &request.environment;
    let record = input.record;
    let PromotionFrom::Environment(from) = &record.from else {
        return Ok(Vec::new());
    };
    let source_fields = source_fields_of(context);

    // What this promotion writes, with its source commits.
    let mut writes: BTreeMap<Item, Option<supersede::Commits>> = BTreeMap::new();
    if let Some(commit) = &record.source_commit {
        writes.insert(Item::Source, Some(supersede::source_commits(commit)));
    }
    for (name, value) in &record.values {
        let state_commit = value
            .state
            .as_ref()
            .map_or(&from.desired_commit, |state| &state.desired_commit);
        let commits = input
            .history
            .iter()
            .find(|state| state.desired_commit == *state_commit)
            .and_then(|state| supersede::value_commits(&value.select, &state.snapshot.desired, &source_fields));
        writes.insert(Item::Value(name.clone()), commits);
    }

    let records = records_to_compare(context, request, input, &writes)?;
    let commits_of = record_commits(context, request, input, &records, &writes)?;
    let mut plan = supersede::plan(&record.path, &writes, &commits_of);
    report.warnings.extend(plan.unchecked.iter().cloned());
    if plan.obligations.is_empty() {
        return Ok(Vec::new());
    }
    if context.sources.is_shallow() {
        return Err(Finding::new(
            Code::PromoteShallowClone,
            format!(
                "the checkout is a shallow clone, so nyl promote cannot compare what path {} promotes with what other paths promoted into {target}",
                record.path
            ),
            "fetch the full history (`git fetch --unshallow`, or `fetch-depth: 0` with actions/checkout) and promote again",
        ));
    }
    let answers = answer_checks(context, target, &mut plan)?;
    if answers
        .values()
        .all(|answer| matches!(answer, supersede::Answer::Missing(missing) if missing.is_empty()))
    {
        return Ok(Vec::new());
    }
    let entries = resolve_entries(context, options, &plan, &answers)?;
    let judged = supersede::Judged {
        path: &record.path,
        source: input.source,
        target,
        by: &options.requested.by,
        names: &options.repository_names,
    };
    match supersede::judge(&judged, &plan, &answers, &entries) {
        Ok(accepted) => {
            report.warnings.extend(accepted.unchecked);
            Ok(accepted.superseded)
        }
        Err(refusal) => {
            report.missing = missing_report(options, &refusal.unsatisfied);
            Err(refusal.finding)
        }
    }
}

/// What a supersede refusal lists in the report.
fn missing_report(options: &PromoteOptions, unsatisfied: &[supersede::Unsatisfied]) -> Vec<PromoteMissing> {
    unsatisfied
        .iter()
        .map(|unsatisfied| PromoteMissing {
            path: unsatisfied.path.to_string(),
            sequence: unsatisfied.sequence,
            repository: options.repository_names.name_of(&unsatisfied.repository),
            items: unsatisfied.items.iter().map(ToString::to_string).collect(),
            commit: unsatisfied.value.clone(),
            record_commit: unsatisfied.record.clone(),
            supersede: unsatisfied.supersede.clone(),
            commits: unsatisfied
                .missing
                .iter()
                .map(|missing| PromoteMissingCommit {
                    commit: missing.commit.clone(),
                    subject: missing.subject.clone(),
                })
                .collect(),
        })
        .collect()
}

/// Answer each check of `plan`: fetch its commits into the entry clone (a
/// promoted commit must be there; a record's may be missing, which leaves
/// it [`supersede::Answer::Unavailable`]), drop records a newer one of the
/// same path contains by ancestry, and ask Git for the missing commits.
fn answer_checks(
    context: &RunContext<'_>,
    target: &EnvironmentName,
    plan: &mut supersede::Plan,
) -> Result<BTreeMap<supersede::Check, supersede::Answer>, Finding> {
    let unreadable = |error: &dyn std::fmt::Display| {
        Finding::new(
            Code::SourceUnavailable,
            format!("cannot compare the promoted commits with what other paths promoted into {target}: {error}"),
            "make the commits fetchable from this clone (fetch the branches that carry them), then promote again",
        )
    };
    // Which commits are here: a promoted commit must be; a record's may not.
    let mut present: BTreeMap<(SourceRepository, CommitId), bool> = BTreeMap::new();
    let mut fetch = |repository: &SourceRepository, commit: &CommitId| -> Result<bool, Finding> {
        if let Some(known) = present.get(&(repository.clone(), commit.clone())) {
            return Ok(*known);
        }
        let found = context
            .sources
            .fetch_commit(repository, commit)
            .map_err(|error| unreadable(&error))?;
        present.insert((repository.clone(), commit.clone()), found);
        Ok(found)
    };
    for check in plan.checks() {
        if !fetch(&check.repository, &check.value)? {
            return Err(unreadable(&format!(
                "promoted commit {} is not in this clone",
                check.value
            )));
        }
        fetch(&check.repository, &check.record)?;
    }
    // A newer record of the same path that descends from an older one covers it.
    plan.prune(&|repository, older, newer| {
        present.get(&(repository.clone(), older.clone())) == Some(&true)
            && present.get(&(repository.clone(), newer.clone())) == Some(&true)
            && context.sources.is_ancestor(repository, older, newer).unwrap_or(false)
    });
    let mut answers = BTreeMap::new();
    for check in plan.checks() {
        let answer = if present.get(&(check.repository.clone(), check.record.clone())) == Some(&true) {
            supersede::Answer::Missing(
                context
                    .sources
                    .missing_commits(&check.repository, &check.value, &check.record)
                    .map_err(|error| unreadable(&error))?,
            )
        } else {
            supersede::Answer::Unavailable
        };
        answers.insert(check, answer);
    }
    Ok(answers)
}

/// The records the supersede check compares with: the target's current
/// ones, and those at each `promote` commit of its history, this path's own
/// included, so "since" is computed over all of them. When this path's
/// current record carries every item the promotion writes, nothing older
/// than it can count, and only newer history is read.
fn records_to_compare(
    context: &RunContext<'_>,
    request: &RunRequest,
    input: &SupersedeInput<'_>,
    writes: &BTreeMap<Item, Option<supersede::Commits>>,
) -> Result<Vec<PromotionRecord>, Finding> {
    let store = context.base.store;
    let target = &request.environment;
    let record = input.record;
    let own = input
        .current
        .get(&record.path)
        .filter(|own| {
            writes.keys().all(|item| match item {
                Item::Source => own.source_commit.is_some(),
                Item::Value(name) => own.values.contains_key(name),
            })
        })
        .map_or(0, |own| own.sequence);
    let mut records: Vec<PromotionRecord> = input.current.values().cloned().collect();
    let earlier: Vec<(CommitId, u64)> = input
        .target_history
        .desired
        .iter()
        .filter_map(|commit| {
            let summary = recorded_summary(&commit.message, target)?;
            let path = summary.get("path")?.as_str()?;
            let sequence = summary.get("sequence")?.as_u64()?;
            let known = records
                .iter()
                .any(|record| record.path.as_str() == path && record.sequence == sequence);
            (sequence > own && !known).then(|| (commit.commit.clone(), sequence))
        })
        .collect();
    for batch in earlier.chunks(16) {
        let tips: Vec<StateTips> = batch
            .iter()
            .map(|(commit, _)| StateTips {
                desired: Some(commit.clone()),
                observed: None,
            })
            .collect();
        let snapshots = store
            .snapshots_at(&request.location, &tips)
            .map_err(|error| store_error(target, &error))?;
        for ((_, sequence), snapshot) in batch.iter().zip(snapshots) {
            if let Some(found) = snapshot
                .promotions
                .into_values()
                .find(|found| found.sequence == *sequence)
            {
                records.push(found);
            }
        }
    }
    Ok(records)
}

/// Each record's items with their source commits, reading the source states
/// other paths' values came from. Only a value the check compares is read:
/// one on another path, after [`supersede::since`] for it. A state its
/// environment's history no longer holds, or an environment no longer
/// declared, leaves the value's commits unknown; any other failure to read
/// state is an error.
fn record_commits(
    context: &RunContext<'_>,
    request: &RunRequest,
    input: &SupersedeInput<'_>,
    records: &[PromotionRecord],
    writes: &BTreeMap<Item, Option<supersede::Commits>>,
) -> Result<Vec<RecordCommits>, Finding> {
    let store = context.base.store;
    let record = input.record;
    let source_fields = source_fields_of(context);
    // Items first, without commits, so "since" sees every record.
    let mut commits_of: Vec<RecordCommits> = records
        .iter()
        .map(|other| {
            let mut items = BTreeMap::new();
            if let Some(commit) = &other.source_commit {
                items.insert(Item::Source, Some(supersede::source_commits(commit)));
            }
            for name in other.values.keys() {
                if writes.contains_key(&Item::Value(name.clone())) {
                    items.insert(Item::Value(name.clone()), None);
                }
            }
            RecordCommits {
                path: other.path.clone(),
                sequence: other.sequence,
                items,
            }
        })
        .collect();
    let since: BTreeMap<Item, u64> = writes
        .keys()
        .map(|item| (item.clone(), supersede::since(&record.path, item, &commits_of)))
        .collect();
    let mut histories: BTreeMap<EnvironmentName, Option<BTreeSet<CommitId>>> = BTreeMap::new();
    let mut states: BTreeMap<(EnvironmentName, CommitId), Option<EnvironmentSnapshot>> = BTreeMap::new();
    for (other, commits) in records.iter().zip(&mut commits_of) {
        let PromotionFrom::Environment(other_from) = &other.from else {
            continue;
        };
        if other.path == record.path {
            continue;
        }
        for (name, value) in &other.values {
            let item = Item::Value(name.clone());
            if !writes.contains_key(&item) || other.sequence <= since[&item] {
                continue;
            }
            let state_commit = value
                .state
                .as_ref()
                .map_or(&other_from.desired_commit, |state| &state.desired_commit);
            let environment = &other_from.environment;
            let key = (environment.clone(), state_commit.clone());
            if !states.contains_key(&key) {
                let location = request.environments.get(environment);
                let held = match (histories.get(environment), location) {
                    (Some(held), _) => held.clone(),
                    (None, None) => None,
                    (None, Some(location)) => {
                        let history = store
                            .history(location)
                            .map_err(|error| store_error(environment, &error))?;
                        Some(history.desired.into_iter().map(|commit| commit.commit).collect())
                    }
                };
                histories.insert(environment.clone(), held.clone());
                let snapshot = match (held, location) {
                    (Some(held), Some(location)) if held.contains(state_commit) => store
                        .snapshots_at(
                            location,
                            &[StateTips {
                                desired: Some(state_commit.clone()),
                                observed: None,
                            }],
                        )
                        .map_err(|error| store_error(environment, &error))?
                        .pop(),
                    _ => None,
                };
                states.insert(key.clone(), snapshot);
            }
            let found = states[&key]
                .as_ref()
                .and_then(|snapshot| supersede::value_commits(&value.select, &snapshot.desired, &source_fields));
            commits.items.insert(item, found);
        }
    }
    Ok(commits_of)
}

/// The `--supersede` entries resolved: each in the repositories the check
/// compares in (a qualified one in its own), keeping the repository whose
/// missing commits it names ([`supersede::match_entry`]). A full commit ID
/// that is not in this clone, such as a record's commit that cannot be
/// fetched, stands for itself.
fn resolve_entries(
    context: &RunContext<'_>,
    options: &PromoteOptions,
    plan: &supersede::Plan,
    answers: &BTreeMap<supersede::Check, supersede::Answer>,
) -> Result<Vec<SupersedeEntry>, Finding> {
    let wanted = supersede::wanted(plan, answers);
    let repositories: std::collections::BTreeSet<SourceRepository> = wanted.keys().cloned().collect();
    let mut entries = Vec::new();
    for entry in &options.supersede {
        let reason = entry
            .reason
            .clone()
            .or_else(|| options.requested.reason.clone())
            .unwrap_or_default();
        let candidates: Vec<SourceRepository> =
            match supersede::qualified(entry, &repositories, &options.repository_names)? {
                Some(repository) => vec![repository],
                None => repositories.iter().cloned().collect(),
            };
        let mut resolved = BTreeMap::new();
        let mut errors = Vec::new();
        for repository in candidates {
            match context.sources.commits_in(&repository, &entry.range) {
                Ok(commits) => {
                    resolved.insert(repository, commits);
                }
                Err(_) if CommitId::new(entry.range.to_ascii_lowercase()).is_ok() => {
                    let commit = CommitId::new(entry.range.to_ascii_lowercase()).expect("checked");
                    resolved.insert(repository, vec![commit]);
                }
                Err(error) => errors.push(error.to_string()),
            }
        }
        if resolved.is_empty() {
            return Err(Finding::new(
                Code::SourceUnavailable,
                format!("--supersede {} names no commits: {}", entry.text(), errors.join("; ")),
                "pass a commit or a range `<from>..<to>` of the missing commits, as the refusal prints it",
            ));
        }
        if let Some(entry) = supersede::match_entry(entry, reason, &resolved, &wanted, &options.repository_names)? {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Why a `record: source` block was not committed, as the finding the
/// promotion is refused with. Nothing was written, except for a push that
/// may have reached the remote (`NYL-PROMOTE-SOURCE-UNCERTAIN`, exit 2) and
/// a commit that could not be undone, which keep the commit on the local
/// branch. No marker is written into the target's state in any case: after
/// an uncertain push, the re-run the fix names writes it once the pushed
/// block is current.
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
        RecordWriteError::PushUnknown { commit, .. } => (
            Code::PromoteSourceUncertain,
            format!("fetch the branch (`git fetch`) and check whether commit {commit} reached the remote, then {again}: once the remote has it and the checkout is at it, the block is unchanged and only the marker is written; if it did not arrive, push it (`git push`) or drop it (`git reset --keep @{{upstream}}`) first"),
        ),
        RecordWriteError::UndoFailed { commit, .. } => (
            Code::PromoteSourceUnwritable,
            format!("nothing was pushed, but the entry worktree still holds commit {commit}: drop it (`git reset --keep @{{upstream}}`) and {again}"),
        ),
        RecordWriteError::Unsigned(_) => (
            Code::PromoteSourceUnwritable,
            format!("sign commits with an SSH key the Environment's allowed signers file lists (`git config commit.gpgsign true`, `gpg.format ssh`, `user.signingkey <key>`), and {again}"),
        ),
        RecordWriteError::Failed(_) => (Code::PromoteSourceUnwritable, format!("fix the cause, then {again}")),
    };
    let message = match error {
        RecordWriteError::PushUnknown { .. } => format!(
            "the promotion record of {target} was committed to its source, but whether the push reached the remote is unknown, so no marker was written into {target}'s state: {error}"
        ),
        RecordWriteError::UndoFailed { .. } => {
            format!("the promotion record of {target} cannot be committed to its source: {error}")
        }
        _ => format!("the promotion record of {target} cannot be committed to its source, so nothing was written: {error}"),
    };
    Finding::new(code, message, fix)
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
    (entry, verifier): (
        &nyl_core::resources::environment::Environment,
        Option<&dyn PromotionBlockVerifier>,
    ),
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
    // Trusted by its signature: checked against its path, and the source's
    // state is not read.
    if let Some(signed_by) = promoted.and_then(|promoted| promoted.verify.as_ref()) {
        crate::rules::promotion::check_block_shape(block, path)?;
        let signature = verifier
            .ok_or_else(|| {
                BlockSignatureError::Unreadable("this invocation cannot read the project's Git history".to_owned())
            })
            .and_then(|verifier| verifier.verify_block(environment, &signed_by.allowed_signers));
        return match signature {
            Ok(_) => Ok(read.records),
            Err(error) => Err(unsigned_block(environment, &signed_by.allowed_signers, &error)),
        };
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

/// Why a `record: source` block is not trusted by its signature
/// (`NYL-PROMOTION-RECORD-UNSIGNED`, exit 1).
fn unsigned_block(environment: &str, allowed_signers: &str, error: &BlockSignatureError) -> Finding {
    let fix = match error {
        BlockSignatureError::Untrusted { .. } => format!(
            "promote again with commit signing on (`git config commit.gpgsign true`, `gpg.format ssh`, and `user.signingkey`) as a signer {allowed_signers} lists, or add the signer's key to {allowed_signers} in a reviewed change; the block is written by `nyl promote`, never by hand"
        ),
        BlockSignatureError::Unreadable(_) => format!(
            "run from a full clone (`fetch-depth: 0`, or `git fetch --unshallow`) with Git 2.34 or newer and OpenSSH 8.2 or newer, the Environment's file committed, and {allowed_signers} present; or remove `verify` to check the block against the source environment's state instead"
        ),
    };
    Finding::new(
        Code::PromotionRecordUnsigned,
        format!("the promoted block of Environment {environment} cannot be trusted by its signature: {error}"),
        fix,
    )
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

/// What reading the target's definitions at a source commit needs.
pub struct TargetSources<'a> {
    pub sources: &'a dyn SourceResolver,
    pub renderer: &'a dyn SourceRenderer,
    /// A directory the invocation owns; the checkout is made beneath it.
    pub workspace: &'a Path,
}

/// Decide with the units `target` selects for the candidate states of
/// `history`, the one way `nyl promote` and `nyl get promotion-candidates`
/// take them: `decide` is [`promotion::decide`] over an input carrying the
/// units given.
///
/// - A target whose source is promoted along the path (`promotes_source`)
///   runs the chosen state's source commit, so its own definitions at that
///   commit decide which units it selects, whatever the entry worktree has
///   checked out ([`TargetUnits::AtSource`]). They are read lazily, newest
///   state first: the decision is taken with what was read, and the source
///   commit of the newest candidate state at or above the chosen one (all
///   of them for a refusal) that is not read yet is checked out, into one
///   reused worktree, rendered, and the decision taken again. A state whose
///   commit is not read cannot qualify, so the decision is settled once
///   every state up to the chosen one is read: one render for the usual
///   promotion of the newest state, not one per recorded state.
/// - A commit whose definitions do not render, such as one before the
///   target was declared, keeps its states out with the reason. So does a
///   commit this clone cannot check out, such as one of a deleted branch;
///   it is also listed in [`TargetUnitsRead::unavailable`], because passing
///   over a newer state for that reason must not promote an older one
///   ([`passed_over`]).
/// - Any other target runs its own source: the units its entry worktree
///   selects ([`TargetUnits::Fixed`]).
pub fn settle_target_units(
    access: &TargetSources<'_>,
    target: &EnvironmentName,
    (promotes_source, entry_units): (bool, &BTreeSet<UnitName>),
    (history, select): (&[RecordedState], &Selection),
    decide: impl Fn(&TargetUnits) -> Result<Decision, Refusal>,
) -> (Result<Decision, Refusal>, TargetUnitsRead) {
    let mut read = TargetUnitsRead {
        units: if promotes_source {
            TargetUnits::AtSource(BTreeMap::new())
        } else {
            TargetUnits::Fixed(entry_units.clone())
        },
        unavailable: BTreeMap::new(),
    };
    let worktree = access.workspace.join("target-units");
    loop {
        let decision = decide(&read.units);
        let TargetUnits::AtSource(known) = &read.units else {
            return (decision, read);
        };
        let chosen = match &decision {
            Ok(Decision::Unchanged(record)) => Some(&record.from),
            Ok(Decision::Promote(promotion)) => Some(&promotion.record.from),
            Err(_) => None,
        }
        .and_then(|from| match from {
            PromotionFrom::Environment(from) => Some(&from.desired_commit),
            PromotionFrom::Target(_) => None,
        });
        let horizon = chosen
            .and_then(|chosen| history.iter().position(|state| state.desired_commit == *chosen))
            .map_or(history.len(), |position| position + 1);
        let next = history[..horizon]
            .iter()
            .filter(|state| state.is_candidate() && select.admits(state))
            .filter_map(RecordedState::source_commit)
            .find(|commit| !known.contains_key(*commit))
            .cloned();
        let Some(commit) = next else {
            return (decision, read);
        };
        let units = read_target_units(access, target, &worktree, &commit, &mut read.unavailable);
        if let TargetUnits::AtSource(known) = &mut read.units {
            known.insert(commit, units);
        }
    }
}

/// The units `target` selects at `commit`, or why they cannot be told.
fn read_target_units(
    access: &TargetSources<'_>,
    target: &EnvironmentName,
    worktree: &Path,
    commit: &CommitId,
    unavailable: &mut BTreeMap<CommitId, Finding>,
) -> Result<BTreeSet<UnitName>, String> {
    if let Err(error) = access.sources.reset_worktree(&SourceRepository::This, commit, worktree) {
        let message =
            format!("cannot check out source commit {commit} to read which units {target} selects there: {error}");
        unavailable.insert(
            commit.clone(),
            Finding::new(
                Code::SourceUnavailable,
                message.clone(),
                "make the commit fetchable from this clone (fetch the branch that carries it), then try again",
            ),
        );
        return Err(message);
    }
    access
        .renderer
        .render(worktree, target, commit)
        .map(|rendered| {
            rendered
                .units
                .into_iter()
                .filter(|(_, unit)| unit.selected)
                .map(|(name, _)| name)
                .collect()
        })
        .map_err(|error| format!("{target}'s definitions at source commit {commit} do not render: {error}"))
}

/// What [`settle_target_units`] read.
pub struct TargetUnitsRead {
    pub units: TargetUnits,
    /// The source commits that could not be checked out, with why.
    pub unavailable: BTreeMap<CommitId, Finding>,
}

/// Why a promotion of the state `chosen` (its desired commit) is refused:
/// a newer candidate state was passed over because its source commit could
/// not be checked out, so whether it qualifies is unknown. Older states
/// whose commits are gone, such as those of a deleted hotfix branch, never
/// stand in the way.
pub fn passed_over(
    read: &TargetUnitsRead,
    history: &[RecordedState],
    select: &Selection,
    chosen: &CommitId,
) -> Option<Finding> {
    history
        .iter()
        .take_while(|state| state.desired_commit != *chosen)
        .filter(|state| state.is_candidate() && select.admits(state))
        .find_map(|state| read.unavailable.get(state.source_commit()?).cloned())
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
            running: BTreeMap::new(),
        });
    }
    Ok(states)
}

/// Ask each unit's driver what its running-scoped attestations say in each
/// state: their names, the identity of what the state's receipt produced,
/// and per recorded attestation what it says about each deployment
/// ([`crate::rules::promotion::UnitRunning`]). The rules judge these typed
/// answers and never read the attestations' details (decisions D6 and D8 of
/// the M6 plan). A unit whose kind is not registered says nothing.
pub fn read_running(drivers: &DriverSet, states: &mut [RecordedState]) {
    for state in states {
        for (unit, desired) in &state.snapshot.desired {
            let Some(driver) = drivers.get(&desired.unit.api_version, &desired.unit.kind) else {
                continue;
            };
            let spec = desired.resolved_spec.as_ref().unwrap_or(&desired.spec);
            let names: BTreeSet<String> = driver.running_attestations(spec).into_iter().collect();
            if names.is_empty() {
                continue;
            }
            let publication = state
                .snapshot
                .artifacts
                .get(unit)
                .and_then(|artifacts| driver.receipt_publication(spec, artifacts));
            let verdicts = state
                .attestations
                .units
                .get(unit)
                .or_else(|| state.attestations.stale_receipts.get(unit))
                .into_iter()
                .flatten()
                .filter(|(name, _)| names.contains(*name))
                .filter_map(|(name, attestation)| {
                    driver
                        .deployment_verdicts(spec, attestation)
                        .map(|verdicts| (name.clone(), verdicts))
                })
                .collect();
            state.running.insert(
                unit.clone(),
                UnitRunning {
                    names,
                    publication,
                    verdicts,
                },
            );
        }
    }
}

/// Per value of `path`, the deployment that consumes it (D7): for an input
/// selector, the deployment the selected unit's driver maps the pointer to
/// in the newest state. Any other value is consumed by its unit.
pub fn value_consumers(
    drivers: &DriverSet,
    history: &[RecordedState],
    path: &PromotionPath,
) -> BTreeMap<String, String> {
    let Some(newest) = history.first() else {
        return BTreeMap::new();
    };
    path.spec
        .values
        .iter()
        .filter_map(|(name, value)| {
            let pointer = value.select.input.as_deref()?;
            let unit = value.select.unit.as_ref()?;
            let desired = newest.snapshot.desired.get(unit)?;
            let driver = drivers.get(&desired.unit.api_version, &desired.unit.kind)?;
            let spec = desired.resolved_spec.as_ref().unwrap_or(&desired.spec);
            driver
                .input_consumer(spec, pointer)
                .map(|deployment| (name.clone(), deployment))
        })
        .collect()
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
        let Some(receipt) = observed.and_then(|observed| observed.receipt.as_ref()) else {
            continue;
        };
        // A receipt that is not current still says what was published last.
        let current = receipt_is_current(desired, observed);
        for snapshot in newer {
            for (name, attestation) in snapshot.attestations.get(unit).into_iter().flatten() {
                let names = attestation.execution_key == receipt.execution_key.as_str()
                    && attestation
                        .unit
                        .as_ref()
                        .is_none_or(|subject| Some(&subject.uid) == desired.unit.uid.as_ref());
                if names {
                    let into = if current {
                        &mut found.units
                    } else {
                        &mut found.stale_receipts
                    };
                    into.entry(unit.clone())
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
/// count too, for state moved without its history and for a block kept in
/// source whose marker is missing (written before markers existed, or one
/// that failed); a `record: source` promotion's marker is a `promote`
/// commit like any other. Environments that share
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

    /// A recorded state of dev, run `number`, that ran source commit `source`.
    fn ran(number: u8, source: char) -> RecordedState {
        let snapshot = EnvironmentSnapshot {
            environment: Some(nyl_core::orchestration::state::EnvironmentRecord {
                api_version: StateV1,
                kind: nyl_core::orchestration::state::EnvironmentRecordKind,
                environment: EnvironmentName::new("dev").unwrap(),
                spec: Value::Null,
                source_commit: CommitId::new(source.to_string().repeat(40)).unwrap(),
                source_chosen_by: nyl_core::orchestration::state::SourceChoice::Entry,
                applied_promotion_sequence: None,
                ownership: BTreeMap::new(),
            }),
            ..EnvironmentSnapshot::default()
        };
        RecordedState {
            run: nyl_core::orchestration::ids::RunId::new(format!("00000000-0000-7000-8000-00000000000{number}"))
                .unwrap(),
            desired_commit: CommitId::new(format!("{number}").repeat(40)).unwrap(),
            observed_commit: None,
            snapshot,
            attestations: StateAttestations::default(),
            running: BTreeMap::new(),
        }
    }

    /// A source commit that cannot be checked out stands in the way only
    /// of an older state: promoting the newest state ignores a gone commit
    /// of an older one (a deleted hotfix branch), while choosing an older
    /// state over a newer one whose commit could not be read is refused.
    /// Catches an unreadable commit that silently promotes an older state,
    /// and a deleted branch that blocks every later promotion.
    #[test]
    fn test_only_a_newer_unreadable_state_refuses_a_promotion() {
        let history = vec![ran(3, 'c'), ran(2, 'b'), ran(1, 'a')];
        let unreadable = |commit: char| TargetUnitsRead {
            units: TargetUnits::AtSource(BTreeMap::new()),
            unavailable: BTreeMap::from([(
                CommitId::new(commit.to_string().repeat(40)).unwrap(),
                Finding::new(Code::SourceUnavailable, "gone", "fetch it"),
            )]),
        };
        let chosen = |number: u8| history[3 - usize::from(number)].desired_commit.clone();
        let newest = &Selection::Newest;
        assert!(passed_over(&unreadable('a'), &history, newest, &chosen(3)).is_none());
        assert!(passed_over(&unreadable('a'), &history, newest, &chosen(2)).is_none());
        let refused = passed_over(&unreadable('c'), &history, newest, &chosen(2)).unwrap();
        assert_eq!(refused.code, Code::SourceUnavailable);
        // An explicitly chosen state is not held back by states the selection excludes.
        let explicit = Selection::StateRevision(chosen(2));
        assert!(passed_over(&unreadable('c'), &history, &explicit, &chosen(2)).is_none());
    }
}
