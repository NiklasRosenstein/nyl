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
use nyl_core::orchestration::ids::{CommitId, EnvironmentName, UnitName};
use nyl_core::orchestration::ports::{EnvironmentSnapshot, StateHistory, StateStore};
use nyl_core::orchestration::promotion::{EvidenceLevel, PromotionFrom, PromotionRecord, PromotionRequest};
use nyl_core::orchestration::state::{ApprovalIdentity, SourceChoice, StateLocation};
use nyl_core::orchestration::time::Timestamp;
use nyl_core::resources::promotion::{PathEvidence, PromotionPath};
use serde_json::Value;

use super::local::check_record;
use super::promote::{self, PathReading};
use super::store_error;
use crate::report::{
    AppliedPromotion, PromotionCandidatesReport, PromotionHistoryEntry, PromotionsReport, StatesReport,
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
    /// The units the target selects, as `nyl promote` takes them.
    pub target_units: &'a BTreeSet<UnitName>,
    /// Whether the target's source is `fromPromotion` naming this path.
    pub promotes_source: bool,
    pub now: Timestamp,
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
    let PathReading { snapshot, history } = match promote::read_path(
        store,
        (target, target_location),
        (source, source_location),
        &Selection::Newest,
    ) {
        Ok(reading) => reading,
        Err(finding) => return report.refused(finding),
    };
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
    let input = PromotionInput {
        path,
        source,
        history: &history,
        target_units: request.target_units,
        promotes_source: request.promotes_source,
        target_records: &snapshot.promotions,
        sequence: 0,
        now: request.now,
        request: &ask,
    };
    let evaluations = promotion::evaluate(&input);
    let (selected, refusal) = match promotion::decide(&input) {
        Ok(Decision::Promote(promotion)) => (states_named(&promotion.record), None),
        Ok(Decision::Unchanged(record)) => (states_named(&record), None),
        Err(refusal) => (Vec::new(), Some(refusal.finding)),
    };
    let promoted = path
        .name()
        .ok()
        .and_then(|name| snapshot.promotions.get(&name))
        .map(states_named)
        .unwrap_or_default();
    let mut report = report.with_candidates(evaluations, required, &selected, &promoted);
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
/// one its last run applied, and every promotion its desired history records.
pub fn promotions(store: &dyn StateStore, location: &StateLocation, environment: &EnvironmentName) -> PromotionsReport {
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
    let mut records: Vec<PromotionRecord> = snapshot.promotions.into_values().collect();
    records.sort_by_key(|record| std::cmp::Reverse(record.sequence));
    report.promotions = records;
    report.history = history
        .desired
        .iter()
        .filter_map(|commit| history_entry(&commit.commit, &commit.message))
        .collect();
    report
}

/// One promotion from a `promote` transition commit, as its summary and
/// trailers record it.
fn history_entry(commit: &CommitId, message: &str) -> Option<PromotionHistoryEntry> {
    let summary = promote::recorded_summary(message)?;
    let text = |key: &str| summary.get(key).and_then(Value::as_str).map(str::to_owned);
    Some(PromotionHistoryEntry {
        commit: commit.clone(),
        path: text("path")?,
        sequence: summary.get("sequence")?.as_u64()?,
        source_commit: text("sourceCommit").and_then(|commit| CommitId::new(commit).ok()),
        from: summary.get("from").filter(|from| !from.is_null()).cloned(),
        requested_by: trailer(message, "Nyl-Requested-By").map(|by| by.trim().to_owned()),
        reason: trailer(message, "Nyl-Reason").map(|reason| reason.trim().to_owned()),
    })
}
