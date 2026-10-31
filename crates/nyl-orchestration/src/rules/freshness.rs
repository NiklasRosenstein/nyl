//! Whether a receipt is current.
//!
//! Contract: [Observed unit](../../../../design/orchestration-core.md#observed-unit):
//! a receipt is current exactly when its unit's uid and its execution key
//! equal the current desired document's. Readiness, reference resolution,
//! status, and `nyl get` all ask this one function. Producer identity
//! (the uids in provenance) never makes a receipt stale and is not part of
//! the execution key: a consumer of a recreated producer runs again only
//! when its own execution key changes, such as when it consumes an output
//! that changed with the producer. A returning tombstone with an unchanged
//! key is current at once, and the dependents of a replaced unit with an
//! unchanged key stay current.

use nyl_core::orchestration::state::{DesiredUnit, ObservedUnit};

/// Whether `observed` holds a receipt that is current for `desired`. A
/// desired document without an execution key (blocked) has no current
/// receipt, and neither does one whose incarnation differs.
pub fn receipt_is_current(desired: &DesiredUnit, observed: Option<&ObservedUnit>) -> bool {
    let Some(observed) = observed else {
        return false;
    };
    if observed.operator_modified.is_some()
        && !matches!(
            desired.lifecycle,
            nyl_core::orchestration::state::Lifecycle::Held { .. }
        )
    {
        return false;
    }
    let (Some(key), Some(receipt)) = (&desired.execution_key, &observed.receipt) else {
        return false;
    };
    desired.unit.uid.is_some() && desired.unit.uid == observed.unit.uid && receipt.execution_key == *key
}

/// `observed` when it is of `desired`'s incarnation: a receipt or condition
/// applies only to the incarnation it was recorded for.
pub(crate) fn same_incarnation<'o>(
    desired: &DesiredUnit,
    observed: Option<&'o ObservedUnit>,
) -> Option<&'o ObservedUnit> {
    observed.filter(|observed| observed.unit.uid == desired.unit.uid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::fixtures::{desired_numbered, hex_key, observed_numbered};

    /// Catches a matching receipt not counting as current, which would
    /// re-execute every unit on every run.
    #[test]
    fn test_receipt_is_current_when_uid_and_key_match() {
        assert!(receipt_is_current(
            &desired_numbered("network", 1, Some(hex_key('a'))),
            Some(&observed_numbered("network", 1, Some(hex_key('a'))))
        ));
    }

    /// Catches a stale receipt counting as current: another incarnation's,
    /// one for different inputs, one for a blocked unit, or none at all.
    #[test]
    fn test_receipt_is_not_current_on_any_mismatch() {
        let current = desired_numbered("network", 1, Some(hex_key('a')));
        assert!(!receipt_is_current(
            &current,
            Some(&observed_numbered("network", 2, Some(hex_key('a'))))
        ));
        assert!(!receipt_is_current(
            &current,
            Some(&observed_numbered("network", 1, Some(hex_key('b'))))
        ));
        assert!(!receipt_is_current(
            &desired_numbered("network", 1, None),
            Some(&observed_numbered("network", 1, Some(hex_key('a'))))
        ));
        assert!(!receipt_is_current(
            &current,
            Some(&observed_numbered("network", 1, None))
        ));
        assert!(!receipt_is_current(&current, None));
    }
}
