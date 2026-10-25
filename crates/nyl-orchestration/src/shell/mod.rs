//! The orchestration shell: the only code that performs effects, and only
//! through the ports of [`nyl_core::orchestration::ports`].
//!
//! Decisions come from [`crate::rules`]; the shell reads snapshots, asks the
//! rules, and writes what they decide.

pub mod inspect;
pub mod lease;
pub mod local;
pub mod recover;
pub mod run;
pub mod state;
pub mod verify;

use nyl_core::orchestration::ports::{Clock, IdGenerator, StateStore};

use crate::fault::FaultInjector;
use nyl_core::orchestration::time::Duration;

/// Defaults of `nyl.toml` that runs use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Deadline of resolution and of operations without units.
    pub resolution_timeout: Duration,
    /// Added to every deadline; absorbs clock skew.
    pub lease_grace: Duration,
    /// How often a run waiting for a lease reads it again.
    pub lease_poll: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resolution_timeout: Duration::from_mins(30),
            lease_grace: Duration::from_mins(10),
            lease_poll: Duration::from_secs(15),
        }
    }
}

/// What every operation of the shell runs with.
pub struct Context<'a> {
    pub store: &'a dyn StateStore,
    pub clock: &'a dyn Clock,
    pub ids: &'a dyn IdGenerator,
    /// Fault and pause points; [`crate::fault::NoFaults`] in production.
    pub faults: &'a dyn FaultInjector,
    /// Who runs it, such as a CI run URL or `user@host`.
    pub runner: String,
    /// Who requested the operation, recorded as `Nyl-Requested-By`.
    pub requested_by: String,
    /// Nyl's version, recorded as `Nyl-Version`.
    pub version: String,
    pub settings: Settings,
}
