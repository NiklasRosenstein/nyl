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
