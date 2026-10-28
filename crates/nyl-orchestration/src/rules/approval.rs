//! Invocation approvals and their binding to the initial desired document.
//!
//! Contract: [Approval](../../../../design/orchestration-core.md#approval).

use nyl_core::orchestration::ids::{ChangeDigest, ExecutionKey, UnitName};
use nyl_core::orchestration::state::{ApprovalRecord, DesiredUnit};
use nyl_core::resources::units::{ApprovalBind, ApprovalPolicy, CommonUnitFields};
use std::collections::BTreeMap;

/// Approvals supplied by one invocation, with their audit identity.
#[derive(Debug, Clone, Default)]
pub struct Approvals {
    /// Explicit units, optionally bound to a reviewed change digest.
    pub units: BTreeMap<UnitName, Option<ChangeDigest>>,
    /// Approve every candidate, except policies that require a digest.
    pub all: bool,
    /// Identity resolved before any effects. Its digest is filled per unit.
    pub identity: Option<ApprovalRecord>,
}

/// The common approval policy, already validated during source admission.
pub fn policy(unit: &DesiredUnit) -> ApprovalPolicy {
    serde_json::from_value::<CommonUnitFields>(unit.spec.clone())
        .map(|common| common.approval)
        .unwrap_or_default()
}

impl Approvals {
    /// Record approvals whose desired binding still matches the invocation's
    /// first resolution. A blocked initial document cannot approve a later key.
    pub fn records(
        &self,
        initial: &BTreeMap<UnitName, Option<ExecutionKey>>,
        units: &BTreeMap<UnitName, DesiredUnit>,
    ) -> BTreeMap<UnitName, ApprovalRecord> {
        units
            .iter()
            .filter_map(|(name, unit)| {
                let digest = match self.units.get(name) {
                    Some(digest) => digest.clone(),
                    None if self.all => None,
                    None => return None,
                };
                let policy = policy(unit);
                if policy.require_digest && digest.is_none() {
                    return None;
                }
                if policy.bind == ApprovalBind::Desired
                    && (unit.execution_key.is_none() || initial.get(name) != Some(&unit.execution_key))
                {
                    return None;
                }
                let mut record = self.identity.clone()?;
                record.reviewed = digest.is_some();
                record.digest = digest;
                Some((name.clone(), record))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::fixtures::*;
    use nyl_core::orchestration::state::ApprovalIdentity;
    use serde_json::json;

    #[test]
    fn test_approvals_bind_initial_desired_and_require_reviewed_digest() {
        let a = desired("a", "Command", json!({"approval": "manual"}), "a");
        let mut b = desired(
            "b",
            "Command",
            json!({"approval": {"mode": "manual", "bind": "plan", "requireDigest": true}}),
            "b",
        );
        let mut units = BTreeMap::from([(name("a"), a), (name("b"), b.clone())]);
        let initial = units
            .iter()
            .map(|(name, unit)| (name.clone(), unit.execution_key.clone()))
            .collect();
        let mut approvals = Approvals {
            all: true,
            identity: Some(ApprovalRecord {
                by: vec!["alice".into()],
                identity: ApprovalIdentity::Asserted,
                source: None,
                digest: None,
                reviewed: false,
            }),
            ..Approvals::default()
        };
        assert_eq!(
            approvals.records(&initial, &units).keys().cloned().collect::<Vec<_>>(),
            vec![name("a")]
        );
        units.get_mut(&name("a")).unwrap().execution_key = Some(key("changed"));
        assert!(approvals.records(&initial, &units).is_empty());
        let digest = ChangeDigest::new(format!("sha256:{}", "a".repeat(64))).unwrap();
        approvals.units.insert(name("b"), Some(digest.clone()));
        b.execution_key = Some(key("changed"));
        units.insert(name("b"), b);
        let records = approvals.records(&initial, &units);
        assert_eq!(records[&name("b")].digest, Some(digest));
        assert!(records[&name("b")].reviewed);
    }
    #[test]
    fn test_desired_approval_does_not_authorize_an_initially_blocked_unit() {
        let mut unit = desired("consumer", "Command", json!({"approval":"manual"}), "consumer");
        let initial = BTreeMap::from([(name("consumer"), None)]);
        let approvals = Approvals {
            units: BTreeMap::from([(name("consumer"), None)]),
            identity: Some(ApprovalRecord {
                by: vec!["alice".into()],
                identity: ApprovalIdentity::Asserted,
                source: None,
                digest: None,
                reviewed: false,
            }),
            ..Approvals::default()
        };
        unit.execution_key = None;
        let mut units = BTreeMap::from([(name("consumer"), unit)]);
        assert!(approvals.records(&initial, &units).is_empty());
        units.get_mut(&name("consumer")).unwrap().execution_key = Some(key("resolved"));
        assert!(approvals.records(&initial, &units).is_empty());
    }
}
