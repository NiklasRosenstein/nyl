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

/// [`receipt_is_current`] within an environment's desired documents:
/// additionally, every producer of the same environment that the receipt's
/// provenance cites still has the cited uid in `units`, so a consumer whose
/// producer became a new incarnation (replaced, or returned after its
/// teardown) runs again with the new outputs (open question Q3, option B).
/// Selection, status, `nyl get output`, and the run path ask this one
/// function.
///
/// `units` is the resolution view: every desired document of the
/// environment's state, active, held, deleting, and retained alike. A held
/// or deleting producer keeps its uid, so its consumers stay current; only
/// a new incarnation or a removed producer makes them stale.
///
/// Skeleton note: until package `lifecycle` implements the provenance part,
/// it equals [`receipt_is_current`].
pub fn receipt_is_current_in(
    units: &std::collections::BTreeMap<nyl_core::orchestration::ids::UnitName, DesiredUnit>,
    desired: &DesiredUnit,
    observed: Option<&ObservedUnit>,
) -> bool {
    let _ = units;
    receipt_is_current(desired, observed)
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
