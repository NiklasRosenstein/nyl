//! When a publication's teardown wait after phase 1 ends.
//!
//! Contract: [Kubernetes publication unit](../../../../design/orchestration-core.md#kubernetes-publication-unit),
//! Teardown: `teardownWait` strategies `observe` (Argo CD no longer reports
//! the target's workload Applications within the unit's `timeout`, else
//! uncertain with `applications-remain` or `observation-failed`; default for
//! `mode: observe`), `delay` (`duration` after the phase 1 commit, recorded
//! as not observed), and `manual` (an operator's `--confirm-removed`; the
//! default for `mode: publish`; never keeps a run waiting). A confirmation
//! bound to the run and phase 1 commit, or `--allow-incomplete`, ends any
//! wait once phase 1 is published. A waiting run checks its signal ref about
//! every [`SIGNAL_POLL`].

use nyl_core::orchestration::driver::WaitCompletion;
use nyl_core::orchestration::ports::ApplicationsObservation;
use nyl_core::orchestration::state::{Signal, TeardownWaiting};
use nyl_core::orchestration::time::{Duration, Timestamp};
use serde_json::Value;

/// How often a waiting run reads its signal ref.
pub const SIGNAL_POLL: Duration = Duration::from_secs(15);

/// A publication's `teardownWait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Observe,
    Delay { duration: Duration },
    Manual,
}

/// The strategy a stored publication spec declares, with the defaults by
/// `mode` (`observe` for `mode: observe`, `manual` for `mode: publish`).
pub fn strategy(spec: &Value) -> Strategy {
    let _ = spec;
    todo!("publication")
}

/// When a wait ends by itself: `since + duration` for `delay`, `None` for
/// `observe` and `manual`. The run path sets the phase deadline from this,
/// not from `Outcome::Waiting.until`: only the recorded `since` (the
/// phase 1 commit's time, which the run records) is authoritative, so a
/// driver may leave `until` empty.
pub fn ends(strategy: Strategy, waiting: &TeardownWaiting) -> Option<Timestamp> {
    let _ = (strategy, waiting);
    todo!("publication")
}

/// What deciding a wait reads.
pub struct WaitInputs<'a> {
    pub strategy: Strategy,
    pub waiting: &'a TeardownWaiting,
    pub now: Timestamp,
    /// The unit's teardown deadline (its `timeout` from the teardown's start).
    pub deadline: Timestamp,
    /// The observer's latest answer, for `observe`.
    pub observation: Option<&'a ApplicationsObservation>,
    /// A confirmation sent to the waiting run for this unit, if any arrived.
    pub confirmation: Option<&'a Signal>,
    /// `--allow-incomplete`.
    pub allow_incomplete: bool,
}

/// How a wait continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitDecision {
    /// The wait is over: run phase 2.
    Complete(WaitCompletion),
    /// Keep waiting in this run until about `until`, reading signals meanwhile.
    Poll { until: Timestamp },
    /// Stop waiting in this run: the unit stays `tearing-down`, exit 2.
    Stop { reason: String },
    /// The wait failed to establish removal: the teardown is uncertain.
    Uncertain { reason: String },
}

/// Decide how a wait continues.
pub fn decide(inputs: &WaitInputs<'_>) -> WaitDecision {
    let _ = inputs;
    todo!("publication")
}
