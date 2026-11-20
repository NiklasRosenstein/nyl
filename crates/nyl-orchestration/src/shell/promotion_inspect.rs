//! Reading promotion state without a lease: `nyl get states`, `nyl get
//! promotion-candidates`, and `nyl get promotions`.
//!
//! Contract: [Promoting an environment's source](../../../../design/promotion.md#promoting-an-environments-source),
//! "Inspection", and [PromotionRecord](../../../../design/promotion.md#promotionrecord).
//! Each reads state through the same functions `nyl promote` reads it with
//! ([`promote::read_path`], [`promote::states_of`]) and judges it with the
//! same rules ([`promotion::evaluate`], [`promotion::decide`]), so what
//! these reports show is what a promotion would do. Each history is read
//! after one sync of its refs ([`StateStore::history`]); the documents of
//! the states are then read from what that sync fetched
//! ([`StateStore::snapshots_at`]), so the round trips to a remote do not
//! grow with the history. Nothing is written and no lease is taken.

use std::collections::BTreeSet;

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::driver::DriverSet;
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateHistory, StateStore};
use nyl_core::orchestration::promotion::{EvidenceLevel, PromotionFrom, PromotionRecord, PromotionRequest};
use nyl_core::orchestration::state::{ApprovalIdentity, SourceChoice, StateLocation};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::environment::PromotedSource;
use nyl_core::resources::promotion::{PathEvidence, PromotionPath};
use serde_json::Value;

use super::local::check_record;
use super::promote::{self, PathReading};
use super::store_error;
use crate::report::{
    AppliedPromotion, BindingSupplier, PromotionCandidatesReport, PromotionHistoryEntry, PromotionsReport, StatesReport,
};
use crate::rules::promotion::{self, Decision, PromotionInput, Request, Selection};
use crate::transition::trailer;

/// The environment's current snapshot and its history, after one sync.
fn read(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
) -> Result<(EnvironmentSnapshot, StateHistory), Finding> {
    let error = |error| store_error(environment, &error);
    let history = store.history(location).map_err(error)?;
    let snapshot = store
        .snapshots_at(location, std::slice::from_ref(&history.tips))
        .map_err(error)?
        .pop()
        .unwrap_or_default();
    check_record(store, snapshot.record.as_ref(), environment, location)?;
    Ok((snapshot, history))
}

/// `nyl get states -e <env>`: the recorded states of the environment, newest
/// first (the newest [`promote::HISTORY_STATES`], as `nyl promote` considers
/// them), with the attestations recorded for each.
pub fn states(store: &dyn StateStore, location: &StateLocation, environment: &EnvironmentName) -> StatesReport {
    let read = || {
        let (_, history) = read(store, location, environment)?;
        promote::states_of(store, location, environment, &history, &Selection::Newest)
    };
    match read() {
        Ok(states) => StatesReport::new(environment.clone(), &states),
        Err(finding) => StatesReport::refused(environment.clone(), finding),
    }
}

/// What `nyl get promotion-candidates <path>` evaluates.
pub struct CandidatesRequest<'a> {
    /// The validated path.
    pub path: &'a PromotionPath,
    pub target: (&'a EnvironmentName, &'a StateLocation),
    pub source: (&'a EnvironmentName, &'a StateLocation),
    /// The units the target's entry worktree selects, which count when the
    /// target does not run the promoted commit ([`promote::target_units`]).
    pub entry_units: &'a BTreeSet<UnitName>,
    /// Where the target's definitions at each candidate's source commit are
    /// read, as `nyl promote` reads them.
    pub target_sources: promote::TargetSources<'a>,
    /// Whether the target's source is `promotion` naming this path.
    pub promotes_source: bool,
    /// The target's `source.promotion` as the entry worktree declares
    /// it: with `record: source`, its block is the path's current record.
    pub promoted: Option<&'a PromotedSource>,
    pub now: Timestamp,
    /// The drivers, which say what the source's running-scoped attestations
    /// show ([`promote::read_running`]).
    pub drivers: &'a DriverSet,
}

/// `nyl get promotion-candidates <path>`: every recorded state of the
/// source judged against the path as `nyl promote` judges it, and the
/// state `nyl promote` without flags would take, or why it would refuse.
pub fn candidates(store: &dyn StateStore, request: &CandidatesRequest<'_>) -> PromotionCandidatesReport {
    let path = request.path;
    let (target, target_location) = request.target;
    let (source, source_location) = request.source;
    let required = match path.spec.evidence {
        PathEvidence::Published => EvidenceLevel::Published,
        PathEvidence::Attested => EvidenceLevel::Attested,
    };
    let report = PromotionCandidatesReport::new(&path.metadata.name, source.clone(), target.clone(), required);
    let PathReading { snapshot, mut history } = match promote::read_path(
        store,
        (target, target_location),
        (source, source_location),
        &Selection::Newest,
    ) {
        Ok(reading) => reading,
        Err(finding) => return report.refused(finding),
    };
    promote::read_running(request.drivers, &mut history);
    let consumers = promote::value_consumers(request.drivers, &history, path);
    // A promotion without flags. Who asks is not read by the decision's
    // choice, and the sequence is not shown: neither is recorded here.
    let ask = Request {
        select: Selection::Newest,
        evidence_published: false,
        ignore: Vec::new(),
        requested: PromotionRequest {
            by: String::new(),
            identity: ApprovalIdentity::Asserted,
            source: None,
            at: request.now,
            reason: None,
        },
    };
    let records = crate::rules::sources::recorded_promotions(&snapshot, request.promoted);
    macro_rules! input {
        ($units:expr) => {
            PromotionInput {
                path,
                source,
                history: &history,
                target_units: $units,
                promotes_source: request.promotes_source,
                consumers: &consumers,
                target_records: &records,
                sequence: 0,
                now: request.now,
                request: &ask,
            }
        };
    }
    // The same lazy reading `nyl promote` takes, so the state it selects is
    // the one `nyl promote` would take; states older than that one whose
    // commits were not read show why.
    let (decision, read_units) = promote::settle_target_units(
        &request.target_sources,
        target,
        (request.promotes_source, request.entry_units),
        (&history, &Selection::Newest),
        |units| promotion::decide(&input!(units)),
    );
    let evaluations = promotion::evaluate(&input!(&read_units.units));
    let (selected, refusal) = match decision {
        Ok(Decision::Promote(promotion)) => (states_named(&promotion.record), None),
        Ok(Decision::Unchanged(record)) => (states_named(&record), None),
        Err(refusal) => (Vec::new(), Some(refusal.finding)),
    };
    // `nyl promote` refuses a state chosen over a newer one it could not read.
    let (selected, refusal) = match selected.first() {
        Some(chosen) => match promote::passed_over(&read_units, &history, &Selection::Newest, chosen) {
            Some(finding) => (Vec::new(), Some(finding)),
            None => (selected, refusal),
        },
        None => (selected, refusal),
    };
    let current = path.name().ok().and_then(|name| records.get(&name));
    let promoted = current.map(states_named).unwrap_or_default();
    // What the target runs: the path's record, when the last run applied it
    // and no newer one waits (the lineage `status` shows).
    let running = promotion::lineage_with(&snapshot, request.promoted)
        .filter(|lineage| {
            lineage.path == path.metadata.name && lineage.pending_sequence.is_none() && lineage.from.is_some()
        })
        .and(current)
        .map(states_named)
        .unwrap_or_default();
    let mut report = report.with_candidates(evaluations, &selected, &promoted, &running);
    report.refusal = refusal;
    report
}

/// The source states a record names: its `from`, and each value's own state.
fn states_named(record: &PromotionRecord) -> Vec<CommitId> {
    let from = match &record.from {
        PromotionFrom::Environment(from) => Some(from.desired_commit.clone()),
        PromotionFrom::Target(_) => None,
    };
    from.into_iter()
        .chain(
            record
                .values
                .values()
                .filter_map(|value| value.state.as_ref().map(|state| state.desired_commit.clone())),
        )
        .collect()
}

/// `nyl get promotions -e <env>`: the environment's PromotionRecords, the
/// one its last run applied, the record that supplies its source and each of
/// its units' `$promotion` bindings (as the binding resolver chooses it,
/// [`crate::rules::sources::binding_supplier`], over the units' desired
/// documents), and every promotion its desired history records.
/// With `record: source`, `promoted` (the Environment's `source.promotion`
/// as the entry worktree declares it) supplies the path's record; its earlier
/// records are commits in source, not in the desired history.
pub fn promotions(
    store: &dyn StateStore,
    location: &StateLocation,
    environment: &EnvironmentName,
    promoted: Option<&PromotedSource>,
) -> PromotionsReport {
    let report = PromotionsReport::new(environment.clone());
    let (snapshot, history) = match read(store, location, environment) {
        Ok(read) => read,
        Err(finding) => return report.refused(finding),
    };
    let mut report = report;
    report.applied = snapshot
        .environment
        .as_ref()
        .and_then(|record| match &record.source_chosen_by {
            SourceChoice::Promotion { path, sequence } => Some(AppliedPromotion {
                path: path.clone(),
                sequence: *sequence,
            }),
            _ => None,
        });
    let recorded = crate::rules::sources::recorded_promotions(&snapshot, promoted);
    // The paths that supply the source: as the entry worktree declares
    // them, else as the last run kept the Environment.
    let source_paths: Option<BTreeSet<String>> = promoted
        .map(|promoted| promoted.path.iter().chain(&promoted.paths).cloned().collect())
        .or_else(|| {
            snapshot
                .environment
                .as_ref()
                .and_then(|record| crate::rules::sources::promoted_source_paths(&record.spec))
        });
    let as_applied = |record: &PromotionRecord| AppliedPromotion {
        path: record.path.to_string(),
        sequence: record.sequence,
    };
    report.source = source_paths.as_ref().and_then(|paths| {
        let paths: Vec<&String> = paths.iter().collect();
        crate::rules::sources::source_supplier(recorded.values(), &paths).map(as_applied)
    });
    for (unit, desired) in &snapshot.desired {
        for (pointer, binding) in crate::rules::promotion_paths::bindings_in(&desired.spec) {
            let supplier =
                crate::rules::sources::binding_supplier(&recorded, source_paths.as_ref(), &binding).map(as_applied);
            report.bindings.push(BindingSupplier {
                unit: unit.clone(),
                pointer,
                value: binding.value,
                path: binding.path,
                supplier,
            });
        }
    }
    let mut records: Vec<PromotionRecord> = recorded.into_values().collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.sequence));
    report.promotions = records;
    report.history = history
        .desired
        .iter()
        .filter_map(|commit| history_entry(&commit.commit, &commit.message, environment))
        .collect();
    report
}

/// One promotion into `environment` from a `promote` transition commit, as
/// its summary and trailers record it. Commits of other operations, and of
/// other environments sharing the desired ref, are none.
fn history_entry(commit: &CommitId, message: &str, environment: &EnvironmentName) -> Option<PromotionHistoryEntry> {
    let summary = promote::recorded_summary(message, environment)?;
    let text = |key: &str| summary.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(PromotionHistoryEntry {
        commit: commit.clone(),
        path: text("path")?,
        sequence: summary.get("sequence")?.as_u64()?,
        source_commit: text("sourceCommit").and_then(|commit| CommitId::new(commit).ok()),
        from: summary
            .get("from")
            .and_then(|from| serde_json::from_value(from.clone()).ok()),
        requested_by: trailer(message, "Nyl-Requested-By").map(|by| by.trim().to_owned()),
        reason: trailer(message, "Nyl-Reason").map(|reason| reason.trim().to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(operation: &str, environment: &str) -> String {
        format!(
            "nyl: {operation} {environment} (dev-to-prod #3)\n\nunits: {{}}\npromotion: {{\"path\":\"dev-to-prod\",\"sequence\":3,\"sourceCommit\":\"{source}\",\"from\":{{\"environment\":\"dev\",\"run\":\"0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90\",\"desiredCommit\":\"{desired}\"}}}}\n\nNyl-Operation: {operation}\nNyl-Run-Id: r\nNyl-Environment: {environment}\nNyl-Requested-By: ci\nNyl-Reason: Release 3\n",
            source = "5".repeat(40),
            desired = "d".repeat(40),
        )
    }

    /// The history of an environment lists its own `promote` commits only:
    /// not other operations' commits, and not those of another environment
    /// sharing the desired ref (`spec.state.path`). Catches a neighbour's
    /// promotions listed as this environment's.
    #[test]
    fn test_history_entry_reads_only_this_environments_promotions() {
        let commit = CommitId::new("a".repeat(40)).unwrap();
        let prod = EnvironmentName::new("prod").unwrap();
        let entry = history_entry(&commit, &message("promote", "prod"), &prod).unwrap();
        assert_eq!(entry.path, "dev-to-prod");
        assert_eq!(entry.sequence, 3);
        assert_eq!(entry.source_commit, Some(CommitId::new("5".repeat(40)).unwrap()));
        let from = entry.from.unwrap();
        assert_eq!(from.environment.as_str(), "dev");
        assert_eq!(from.desired_commit, CommitId::new("d".repeat(40)).unwrap());
        assert_eq!(entry.requested_by.as_deref(), Some("ci"));
        assert_eq!(entry.reason.as_deref(), Some("Release 3"));
        assert_eq!(history_entry(&commit, &message("reconcile", "prod"), &prod), None);
        assert_eq!(history_entry(&commit, &message("promote", "staging"), &prod), None);
    }
}
