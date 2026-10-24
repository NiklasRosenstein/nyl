//! Wiring of `plan`, `reconcile`, `status`, `get -e`, and the state
//! commands: everything they take from outside the declaration, injected,
//! so the tier-1 scenario harness runs them in-process with a fake clock,
//! sequential identifiers, fake kinds, and a fixed commit identity.
//!
//! [`Wiring::from_process`] is the production wiring: the process clock,
//! UUIDv7 identifiers, the built-in drivers (plus the fake kinds with the
//! `test-kinds` feature), the process runner, no faults, the runner name
//! from the CI environment, and the admitted process variables.

use std::collections::BTreeMap;
use std::sync::Arc;

use nyl_core::orchestration::driver::{DriverSet, ToolRunner};
use nyl_core::orchestration::ports::{Clock, CommitIdentity, IdGenerator};
use nyl_orchestration::fault::FaultInjector;

use crate::Result;

/// What orchestration commands use beyond their arguments.
pub struct Wiring<'a> {
    pub clock: Arc<dyn Clock>,
    pub ids: &'a dyn IdGenerator,
    pub drivers: &'a DriverSet,
    pub tools: &'a dyn ToolRunner,
    pub faults: &'a dyn FaultInjector,
    /// Who runs Nyl, such as a CI run URL or `user@host`.
    pub runner: String,
    /// The commit identity when no `--author` is given; `None` reads the
    /// clone's Git `user.name` and `user.email`. Resolved only by commands
    /// that write (reconcile, state and lease writes); `plan`, `status`, and
    /// `get` open a read-only store and need none.
    pub author: Option<CommitIdentity>,
    /// A snapshot of the whole process environment, read once here; the
    /// shell admits only `PATH` and each unit's declared passthrough from it.
    pub process_env: BTreeMap<String, String>,
}

/// How one orchestration command ended, without printing or exiting.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandOutcome {
    /// The exit category's code.
    pub exit: u8,
    /// The command's typed report as JSON (`--output json`), or the
    /// persisted documents for `get unit -o json`.
    pub report: serde_json::Value,
    /// What the command prints to stdout in its selected output format.
    pub stdout: String,
    /// Warnings and errors as printed to stderr.
    pub stderr: String,
}

/// Owned production dependencies that [`Wiring`] borrows.
pub struct ProcessWiring {
    pub ids: crate::orchestration::ids::UuidV7,
    pub drivers: DriverSet,
    pub tools: nyl_drivers::process::ProcessRunner,
    pub faults: nyl_orchestration::fault::NoFaults,
}

impl ProcessWiring {
    pub fn new() -> Self {
        todo!("cli-integration")
    }
}

impl Default for ProcessWiring {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Wiring<'a> {
    /// The production wiring over `owned`.
    pub fn from_process(owned: &'a ProcessWiring) -> Result<Self> {
        let _ = owned;
        todo!("cli-integration")
    }
}
