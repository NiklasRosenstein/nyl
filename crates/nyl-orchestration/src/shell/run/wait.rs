//! A run waiting on a publication's teardown after phase 1.
//!
//! Contract: [Kubernetes publication unit](../../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown and the signal path, and [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases)
//! item 2 (a teardown wait's deadline is the wait's end). Decisions come
//! from [`crate::rules::teardown_wait`]; this module performs the waiting:
//! it reads the run's signal ref about every 15 seconds through the
//! injected clock, asks the run context's [`ArgoObserver`](nyl_core::orchestration::ports::ArgoObserver)
//! for `observe`, shows the remaining time and accepts Enter through the
//! [`OperatorPrompt`](nyl_core::orchestration::ports::OperatorPrompt) when
//! there is one, and stops on cancellation. A `manual` wait returns at once.

// Skeleton: package `lifecycle` calls `wait` from the teardown waves; remove
// this allowance once it does.
#![allow(dead_code)]

use nyl_core::orchestration::driver::WaitCompletion;
use nyl_core::orchestration::ids::{EnvironmentName, RunId};
use nyl_core::orchestration::state::{DesiredUnit, StateLocation, TeardownWaiting};
use nyl_core::orchestration::time::Timestamp;

use super::{Cancellation, RunContext};

/// One wait of one unit.
pub(crate) struct WaitRequest<'a> {
    pub location: &'a StateLocation,
    pub environment: &'a EnvironmentName,
    /// The waiting run, whose signal ref confirmations arrive on.
    pub run: &'a RunId,
    /// The unit's stored desired document (its `teardownWait` and target).
    pub unit: &'a DesiredUnit,
    pub waiting: &'a TeardownWaiting,
    /// The end of the unit's teardown deadline; the caller has set the
    /// lease's deadline to cover it.
    pub deadline: Timestamp,
    /// `--allow-incomplete`.
    pub allow_incomplete: bool,
    pub cancellation: &'a Cancellation,
}

/// How a wait ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitEnd {
    /// Run phase 2 with this completion.
    Complete(WaitCompletion),
    /// The wait continues beyond this run: the unit stays `tearing-down` (exit 2).
    Stop { reason: String },
    /// Removal could not be established: the teardown is uncertain.
    Uncertain { reason: String },
    /// SIGINT or SIGTERM.
    Cancelled,
}

/// Wait for `request`'s unit until its wait ends, this run stops waiting,
/// or the run is cancelled.
pub(crate) fn wait(context: &RunContext<'_>, request: &WaitRequest<'_>) -> WaitEnd {
    let _ = (context, request);
    todo!("publication")
}
