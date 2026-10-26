//! Signals to a running run: `--confirm-removed` and `nyl hold`.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! the signal path, and [Deletion](../../../../design/orchestration-core.md#deletion),
//! "Holds freeze a unit", as amended by the part 5 decisions (Q28):
//!
//! - `--confirm-removed`: with no lease held, confirm directly; when the
//!   lease holder waits on that unit's teardown, push a `confirm-removed`
//!   `Signal` to its signal ref naming the operator, reason, time, run ID,
//!   and phase 1 commit, report where it went, and exit 0; when the holder
//!   does anything else, exit 2 as for a held lease. A confirmation for a
//!   unit the run is not waiting on is rejected when it is sent
//!   (`NYL-CONFIRMATION-REJECTED`).
//! - `nyl hold`: with no lease held, hold directly; when a run holds the
//!   lease, push a `hold` signal to its signal ref, report that it was
//!   delivered (`NYL-HOLD-DELIVERED`), and exit 0. The run checks its
//!   signals before starting each unit ([`received`]), starts no held unit,
//!   writes the hold in its own transition commit, and reports the unit as
//!   held.
//! - Either way, after pushing, check that the lease still belongs to that
//!   run; if the run finished meanwhile, delete the signal and act directly
//!   under the invocation's own lease ([`Delivery::Direct`]).

use nyl_core::orchestration::codes::Finding;
use nyl_core::orchestration::ids::{EnvironmentName, RunId, UnitName};
use nyl_core::orchestration::state::{Signal, SignalAction, StateLocation};

use super::Context;

/// Where a signal goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// No run holds the lease (or it finished before reading the signal):
    /// the invocation acts directly, under its own lease.
    Direct,
    /// Delivered to the running run, which acts on it.
    Delivered { run: RunId, runner: String },
}

/// Decide where `action` for `unit` goes, sending the signal when a run
/// holds the lease (for `confirm-removed`, a run waiting on that unit).
pub fn deliver(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    unit: &UnitName,
    action: SignalAction,
    reason: Option<&str>,
) -> Result<Delivery, Finding> {
    let _ = (context, location, environment, unit, action, reason);
    todo!("publication")
}

/// The signals sent to `run` that it has not acted on yet, oldest first.
/// The run path reads them before starting each unit (holds) and while a
/// teardown waits (confirmations); a store failure reads as none.
#[allow(dead_code)] // Skeleton: package `lifecycle` calls it from the run path.
pub(crate) fn received(
    context: &Context<'_>,
    location: &StateLocation,
    environment: &EnvironmentName,
    run: &RunId,
) -> Vec<Signal> {
    let _ = (context, location, environment, run);
    todo!("publication")
}
