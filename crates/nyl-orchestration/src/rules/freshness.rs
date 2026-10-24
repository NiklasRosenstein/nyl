//! Whether a receipt is current.
//!
//! Contract: [Observed unit](../../../../design/orchestration-core.md#observed-unit):
//! a receipt is current exactly when its unit's uid and its execution key
//! equal the current desired document's. Readiness, reference resolution,
//! status, and `nyl get` all ask this one function.

use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit};

/// Whether `observed` holds a receipt that is current for `desired`. A
/// desired document without an execution key (blocked) has no current
/// receipt, and neither does one whose incarnation differs.
pub fn receipt_is_current(desired: &DesiredUnit, observed: Option<&ObservedUnit>) -> bool {
    let Some(observed) = observed else {
        return false;
    };
    let (Some(key), Some(receipt)) = (&desired.execution_key, &observed.receipt) else {
        return false;
    };
    desired.unit.uid.is_some() && desired.unit.uid == observed.unit.uid && receipt.execution_key == *key
}

/// Desired and observed documents for the rules' tests.
#[cfg(test)]
pub(crate) mod fixtures {
    use nyl_core::orchestration::ids::{ExecutionKey, Uid};
    use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit};
    use serde_json::json;

    pub(crate) const RUN: &str = "00000000-0000-7000-8000-00000000000a";

    pub(crate) fn uid(number: u64) -> Uid {
        Uid::new(format!("00000000-0000-7000-8000-{number:012x}")).unwrap()
    }

    pub(crate) fn key(digit: char) -> ExecutionKey {
        ExecutionKey::new(format!("sha256:{}", digit.to_string().repeat(64))).unwrap()
    }

    fn identity(name: &str, uid_number: u64) -> serde_json::Value {
        json!({
            "environment": "dev",
            "name": name,
            "uid": uid(uid_number),
            "apiVersion": "units.gitops.nyl/v1",
            "kind": "Command"
        })
    }

    /// A desired unit of incarnation `uid_number`, with `key`.
    pub(crate) fn desired_named(name: &str, uid_number: u64, key: Option<ExecutionKey>) -> DesiredUnit {
        let mut document = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "DesiredUnit",
            "unit": identity(name, uid_number),
            "lifecycle": {"state": "active"},
            "sourceCommit": "a".repeat(40),
            "sourceRevision": "a".repeat(40),
            "spec": {"command": ["./run.sh"]},
            "readiness": {"state": "ready"}
        });
        if let Some(key) = key {
            document["executionKey"] = json!(key);
        }
        serde_json::from_value(document).unwrap()
    }

    /// An observed unit of incarnation `uid_number`, with a receipt of `key`
    /// when given.
    pub(crate) fn observed_named(name: &str, uid_number: u64, key: Option<ExecutionKey>) -> ObservedUnit {
        let mut document = json!({
            "apiVersion": "gitops.nyl/v1",
            "kind": "ObservedUnit",
            "unit": identity(name, uid_number)
        });
        if let Some(key) = key {
            document["receipt"] = json!({
                "executionKey": key,
                "behaviorVersion": 1,
                "run": RUN,
                "startedAt": "2026-01-01T00:00:00Z",
                "finishedAt": "2026-01-01T00:01:00Z"
            });
        }
        serde_json::from_value(document).unwrap()
    }

    pub(crate) fn desired(uid_number: u64, key: Option<ExecutionKey>) -> DesiredUnit {
        desired_named("network", uid_number, key)
    }

    pub(crate) fn observed(uid_number: u64, key: Option<ExecutionKey>) -> ObservedUnit {
        observed_named("network", uid_number, key)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{desired, key, observed};
    use super::*;

    /// Catches a matching receipt not counting as current, which would
    /// re-execute every unit on every run.
    #[test]
    fn test_receipt_is_current_when_uid_and_key_match() {
        assert!(receipt_is_current(
            &desired(1, Some(key('a'))),
            Some(&observed(1, Some(key('a'))))
        ));
    }

    /// Catches a stale receipt counting as current: another incarnation's,
    /// one for different inputs, one for a blocked unit, or none at all.
    #[test]
    fn test_receipt_is_not_current_on_any_mismatch() {
        let current = desired(1, Some(key('a')));
        assert!(!receipt_is_current(&current, Some(&observed(2, Some(key('a'))))));
        assert!(!receipt_is_current(&current, Some(&observed(1, Some(key('b'))))));
        assert!(!receipt_is_current(
            &desired(1, None),
            Some(&observed(1, Some(key('a'))))
        ));
        assert!(!receipt_is_current(&current, Some(&observed(1, None))));
        assert!(!receipt_is_current(&current, None));
    }
}
