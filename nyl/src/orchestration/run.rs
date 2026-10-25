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

use nyl_core::orchestration::codes::Finding;
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
    /// The built-in drivers (plus the fake kinds with the `test-kinds`
    /// feature, sharing an empty script), the process runner, and no faults.
    pub fn new() -> Self {
        #[cfg(feature = "test-kinds")]
        let drivers = nyl_drivers::registry::with_test_kinds(&nyl_drivers::fake::Script::new());
        #[cfg(not(feature = "test-kinds"))]
        let drivers = nyl_drivers::registry::builtin();
        Self {
            ids: crate::orchestration::ids::UuidV7,
            drivers,
            tools: nyl_drivers::process::ProcessRunner::new(),
            faults: nyl_orchestration::fault::NoFaults,
        }
    }
}

impl Default for ProcessWiring {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> Wiring<'a> {
    /// The production wiring over `owned`.
    ///
    /// This is the only place orchestration commands read the process
    /// environment: the clock (`NYL_TEST_NOW` with the `test-clock`
    /// feature), the runner name from the CI variables, and the snapshot the
    /// shell admits `PATH` and declared passthrough variables from.
    pub fn from_process(owned: &'a ProcessWiring) -> Result<Self> {
        let clock = crate::orchestration::clock::process_clock().map_err(crate::NylError::config)?;
        Ok(Self {
            clock,
            ids: &owned.ids,
            drivers: &owned.drivers,
            tools: &owned.tools,
            faults: &owned.faults,
            runner: crate::orchestration::project::runner(),
            author: None,
            process_env: std::env::vars().collect(),
        })
    }

    /// The commit identity of a writing command: `--author` when given,
    /// else the wiring's author, else the clone's Git `user.name` and
    /// `user.email`; a [`Code::CommitIdentityMissing`] finding when none
    /// names one.
    ///
    /// [`Code::CommitIdentityMissing`]: nyl_core::orchestration::codes::Code::CommitIdentityMissing
    pub fn commit_identity(
        &self,
        git_dir: &std::path::Path,
        author: Option<&str>,
    ) -> std::result::Result<CommitIdentity, Finding> {
        match (author, &self.author) {
            (None, Some(identity)) => Ok(identity.clone()),
            (author, _) => crate::orchestration::project::commit_identity(git_dir, author),
        }
    }
}

impl CommandOutcome {
    /// An outcome that failed before producing a typed report, such as an
    /// unreadable project: exit 1 with the error on stderr.
    pub fn error(message: impl std::fmt::Display) -> Self {
        let message = message.to_string();
        Self {
            exit: 1,
            report: serde_json::json!({ "error": message }),
            stdout: String::new(),
            stderr: format!("error: {message}\n"),
        }
    }
}
