//! The lineage `status` shows for an environment whose source commit a
//! promotion chose.
//!
//! Contract: [Promoting an environment's source](../../../../../design/promotion.md#promoting-an-environments-source)
//! (the PromotionRecord names the source state, and desired state records
//! which path supplied the source commit) and the M6 roadmap's "Show
//! promotion lineage in `status`".
//!
//! [`lineage`] is pure over one snapshot, so `status` reads nothing beyond
//! the snapshot it already reads:
//!
//! - The environment record's `sourceChosenBy: {by: promotion, path,
//!   sequence}` names the record the last run applied, and its
//!   `sourceCommit` the commit it runs.
//! - The path's current record supplies `from` when it is the record that
//!   run applied. With `record: source` that record is the block in source
//!   ([`lineage_with`]). When a newer promotion replaced it and no run applied that
//!   one yet, `from` is left out (history keeps the applied record; `nyl get
//!   promotions` lists it).
//! - `pendingSequence` names the newest record on any path that supplies the
//!   source (`paths`) that no run applied yet: the source the next run takes.
//! - An environment whose source was not chosen by a promotion has none.

use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::SourceChoice;

use crate::run_report::PromotionLineage;

/// The promotion lineage of the state `snapshot` holds, if a promotion chose
/// its source commit.
pub fn lineage(snapshot: &EnvironmentSnapshot) -> Option<PromotionLineage> {
    lineage_with(snapshot, None)
}

/// [`lineage`] with the Environment's `source.promotion` as the entry
/// worktree declares it: with `record: source` the path's current record is
/// the block the last run applied, or a newer block in the entry worktree
/// that no run applied yet ([`crate::rules::sources::recorded_promotions`]).
pub fn lineage_with(
    snapshot: &EnvironmentSnapshot,
    entry: Option<&nyl_core::resources::environment::PromotedSource>,
) -> Option<PromotionLineage> {
    let record = snapshot.environment.as_ref()?;
    let SourceChoice::Promotion { path, sequence } = &record.source_chosen_by else {
        return None;
    };
    let records = crate::rules::sources::recorded_promotions(snapshot, entry);
    let current = records
        .iter()
        .find(|(name, _)| name.as_str() == path)
        .map(|(_, record)| record);
    // Every path that supplies the source counts for a pending record: the
    // Environment as the entry worktree declares it, else as the run kept it.
    let mut paths = entry
        .map(|promoted| promoted.path.iter().chain(&promoted.paths).cloned().collect())
        .or_else(|| crate::rules::sources::promoted_source_paths(&record.spec))
        .unwrap_or_default();
    paths.insert(path.clone());
    let paths: Vec<String> = paths.into_iter().collect();
    let pending = crate::rules::sources::source_supplier(records.values(), &paths)
        .map(|record| record.sequence)
        .filter(|newest| newest > sequence);
    Some(PromotionLineage {
        path: path.clone(),
        sequence: *sequence,
        source_commit: record.source_commit.clone(),
        from: current
            .filter(|current| current.sequence == *sequence)
            .map(|current| current.from.clone()),
        pending_sequence: pending,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nyl_core::orchestration::ids::{CommitId, EnvironmentName, PromotionPathName, RunId};
    use nyl_core::orchestration::promotion::{
        EnvironmentFrom, EvidenceLevel, PromotionEvidence, PromotionFrom, PromotionRecord, PromotionRequest,
    };
    use nyl_core::orchestration::state::{
        ApprovalIdentity, EnvironmentRecord, EnvironmentRecordKind, SourceChoice, StateV1,
    };
    use serde_json::json;

    use super::*;

    fn commit(fill: char) -> CommitId {
        CommitId::new(fill.to_string().repeat(40)).unwrap()
    }

    fn record(sequence: u64, desired: char) -> PromotionRecord {
        record_on("dev-to-prod", sequence, desired)
    }

    fn record_on(path: &str, sequence: u64, desired: char) -> PromotionRecord {
        PromotionRecord {
            path: PromotionPathName::new(path).unwrap(),
            sequence,
            from: PromotionFrom::Environment(EnvironmentFrom {
                environment: EnvironmentName::new("dev").unwrap(),
                run: RunId::new("0192f0c4-8d0a-7c3e-9a51-3c1f0e7d2b90").unwrap(),
                desired_commit: commit(desired),
                observed_commit: None,
            }),
            source_commit: Some(commit('5')),
            values: BTreeMap::new(),
            evidence: PromotionEvidence {
                level: EvidenceLevel::Published,
                attestations: Vec::new(),
                override_: None,
            },
            ignored_attestations: Vec::new(),
            excluded_deployments: std::collections::BTreeMap::new(),
            superseded: Vec::new(),
            artifacts_verified: true,
            requested: PromotionRequest {
                by: "ci".to_owned(),
                identity: ApprovalIdentity::Asserted,
                source: None,
                at: "2026-09-25T12:00:00Z".parse().unwrap(),
                reason: None,
            },
        }
    }

    fn snapshot(chosen_by: SourceChoice, current: Option<PromotionRecord>) -> EnvironmentSnapshot {
        let mut snapshot = EnvironmentSnapshot {
            environment: Some(EnvironmentRecord {
                api_version: StateV1,
                kind: EnvironmentRecordKind,
                environment: EnvironmentName::new("prod").unwrap(),
                spec: json!({}),
                source_commit: commit('5'),
                source_chosen_by: chosen_by,
                applied_promotion_sequence: None,
                ownership: BTreeMap::new(),
            }),
            ..EnvironmentSnapshot::default()
        };
        if let Some(current) = current {
            snapshot.promotions.insert(current.path.clone(), current);
        }
        snapshot
    }

    fn promoted(sequence: u64) -> SourceChoice {
        SourceChoice::Promotion {
            path: "dev-to-prod".to_owned(),
            sequence,
        }
    }

    /// The applied record supplies `from`; a newer record no run applied yet
    /// leaves `from` out and names its sequence, so status never shows the
    /// lineage of a promotion the environment does not run. Catches lineage
    /// read from whatever record is current.
    #[test]
    fn test_lineage_names_the_applied_record_and_a_pending_one() {
        let applied = lineage(&snapshot(promoted(1), Some(record(1, 'd')))).unwrap();
        assert_eq!(applied.path, "dev-to-prod");
        assert_eq!(applied.sequence, 1);
        assert_eq!(applied.source_commit, commit('5'));
        assert_eq!(applied.from, Some(record(1, 'd').from));
        assert_eq!(applied.pending_sequence, None);

        let pending = lineage(&snapshot(promoted(1), Some(record(2, 'e')))).unwrap();
        assert_eq!(pending.sequence, 1);
        assert_eq!(pending.from, None);
        assert_eq!(pending.pending_sequence, Some(2));
    }

    /// With several source paths, a newer record on another path is the
    /// pending one, while `from` still names the applied record of its own
    /// path. Catches a status that hides a hotfix waiting for the next run.
    #[test]
    fn test_lineage_pending_sequence_counts_every_source_path() {
        let mut state = snapshot(promoted(1), Some(record(1, 'd')));
        state.environment.as_mut().unwrap().spec =
            json!({"source": {"promotion": {"paths": ["dev-to-prod", "hotfix-to-prod"]}}});
        let hotfix = record_on("hotfix-to-prod", 2, 'e');
        state.promotions.insert(hotfix.path.clone(), hotfix);
        let lineage = lineage(&state).unwrap();
        assert_eq!(lineage.from, Some(record(1, 'd').from));
        assert_eq!(lineage.pending_sequence, Some(2));

        // A record on a path that does not supply the source is not pending.
        state.environment.as_mut().unwrap().spec = json!({"source": {"promotion": {"path": "dev-to-prod"}}});
        assert_eq!(super::lineage(&state).unwrap().pending_sequence, None);
    }

    /// An environment whose source the entry worktree or a revision chose
    /// has no promotion lineage, and neither has one never reconciled.
    #[test]
    fn test_no_lineage_without_a_promoted_source() {
        assert_eq!(lineage(&snapshot(SourceChoice::Entry, Some(record(1, 'd')))), None);
        assert_eq!(lineage(&EnvironmentSnapshot::default()), None);
    }
}
