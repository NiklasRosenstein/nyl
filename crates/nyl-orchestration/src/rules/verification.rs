//! `nyl verify`: which units it verifies, what it records, and its exit.
//!
//! Contract: [Commands](../../../../design/orchestration-core.md#commands)
//! (`verify` writes one observed commit with the latest observations and
//! driver attestations), [Drivers](../../../../design/orchestration-core.md#drivers)
//! (`verify`), and [Inspection](../../../../design/orchestration-core.md#inspection)
//! (exit categories).
//!
//! - Only units with a current receipt are verified; the others report
//!   `no-receipt` (`NYL-VERIFY-NO-RECEIPT`). A kind that cannot verify is
//!   reported as `unsupported` (`NYL-VERIFY-UNSUPPORTED`), never skipped.
//! - Verification never writes a receipt: it records the latest observation
//!   per type and the driver's attestations, and writes nothing when they
//!   did not change.
//! - Exit: 0 every verified unit clean and its attestations pass; 2 a unit
//!   could not be verified (no current receipt, unsupported, or unknown
//!   drift); 3 drift or a failing attestation; 1 an error. Most severe wins.

use std::collections::{BTreeMap, BTreeSet};

use nyl_core::orchestration::driver::Verification;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::EnvironmentSnapshot;
use nyl_core::orchestration::state::{Attestation, DesiredUnit, Observation, ObservationType, Receipt};
use nyl_core::orchestration::time::Timestamp;

use crate::run_report::{ExitCategory, UnitReport};

/// Whether `verify` verifies a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// It has a current receipt.
    Verify,
    /// It has no current receipt: reported, exit 2.
    NoReceipt,
}

/// The units `verify` covers, with `--unit` limiting them.
pub fn targets(snapshot: &EnvironmentSnapshot, named: Option<&BTreeSet<UnitName>>) -> BTreeMap<UnitName, Target> {
    let _ = (snapshot, named);
    todo!("recover-verify")
}

/// The documents one verification records.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Records {
    pub observations: BTreeMap<ObservationType, Observation>,
    pub attestations: BTreeMap<String, Attestation>,
}

/// The observation and attestation documents of `verification` of `unit`'s
/// current `receipt`, at `now`.
pub fn records(
    environment: &EnvironmentName,
    unit: &DesiredUnit,
    receipt: &Receipt,
    verification: &Verification,
    now: Timestamp,
) -> Records {
    let _ = (environment, unit, receipt, verification, now);
    todo!("recover-verify")
}

/// The exit category one unit of a `VerifyReport` contributes.
pub fn unit_exit(unit: &UnitReport) -> ExitCategory {
    let _ = unit;
    todo!("recover-verify")
}
