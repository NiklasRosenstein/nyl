//! What a hold writes: `nyl hold` applies it directly, and a run that
//! received a hold signal applies it in its own transition commit, so both
//! write the same document.
//!
//! Contract: [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit", and [Recovery](../../../../design/orchestration-core.md#recovery),
//! "Holds on uncertain units", as amended by the part 5 decisions (Q16,
//! Q17, Q19):
//!
//! - No desired document in state: a held marker
//!   ([`DesiredUnit::held_marker`]), no uid, no rendering.
//! - An active unit with a receipt of its incarnation: the desired document
//!   the receipt was produced for, read from the observed unit's copy
//!   ([`ObservedUnit::desired`]), never from commit history; its uid stays
//!   and the newer document is recorded under `hold.pending`. The
//!   condition, if any, stays.
//! - A unit with state but nothing showing it ever ran (no receipt, no
//!   condition, and no leftover run listing it): held without its uid.
//! - A deleting unit pauses and keeps its deletion block.
//! - A retained tombstone is refused (`NYL-HOLD-REFUSED`); a held unit is
//!   unchanged.
//!
//! The lifecycle change itself goes through
//! [`super::lifecycle::transition`] (or [`super::lifecycle::transition_absent`]).

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, UnitName};
use nyl_core::orchestration::ports::RunState;
use nyl_core::orchestration::state::{DesiredUnit, Hold, ObservedUnit};

/// The documents a hold writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HoldWrite {
    /// The unit is already held: nothing is written (exit 0 with a note).
    Unchanged,
    /// Write this desired document; the observed file stays as it is.
    Desired(Box<DesiredUnit>),
}

/// Decide what holding `unit` writes, from the stored desired and observed
/// documents and the leftover run refs.
pub fn hold(
    environment: &EnvironmentName,
    unit: &UnitName,
    desired: Option<&DesiredUnit>,
    observed: Option<&ObservedUnit>,
    runs: &[RunState],
    hold: Hold,
) -> Result<HoldWrite, Finding> {
    let _ = (environment, unit, desired, observed, runs, hold);
    todo!("operator")
}
