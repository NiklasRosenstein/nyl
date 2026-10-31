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
use nyl_core::orchestration::ports::{ArgoObserver, Clock, CommitIdentity, IdGenerator, OperatorPrompt};
use nyl_orchestration::fault::FaultInjector;
use nyl_orchestration::shell::run::Cancellation;

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
    /// Set by the command line's SIGINT/SIGTERM handler for `reconcile`,
    /// `verify`, `recover`, and the state operations ("Runs and leases"
    /// item 7); `tools` forwards
    /// it to running tools. Never set elsewhere.
    pub cancellation: Cancellation,
    /// The terminal Enter confirmation of a run waiting on a teardown; `Some`
    /// only when stdin is a terminal (`from_process` detects it). Tests pass
    /// `None` or a scripted prompt.
    pub prompt: Option<&'a dyn OperatorPrompt>,
    /// The Argo CD observer of `teardownWait: observe`; `None` until M5
    /// brings a real one (an `observe` wait without one is uncertain).
    /// Tests pass a `FakeObserver`.
    pub observer: Option<&'a dyn ArgoObserver>,
    /// Live unit events, delivered on stderr by production wiring.
    pub progress: Option<&'a dyn nyl_core::orchestration::progress::ProgressObserver>,
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
    /// The flag the signal handler sets; `tools` forwards it to running tools.
    pub cancellation: Cancellation,
    /// The terminal Enter confirmation of a waiting teardown; `Some` only
    /// when stdin is a terminal.
    pub prompt: Option<TerminalPrompt>,
}

impl ProcessWiring {
    /// The built-in drivers (plus the fake kinds with the `test-kinds`
    /// feature, sharing an empty script), the process runner, and no faults,
    /// never cancelled.
    pub fn new() -> Self {
        Self::cancelled_by(Cancellation::new())
    }

    /// [`Self::new`], cancelled when `cancellation` is set: the process
    /// runner forwards its signal to every running tool's process group.
    pub fn cancelled_by(cancellation: Cancellation) -> Self {
        #[cfg(feature = "tier2")]
        let drivers = nyl_drivers::registry::with_fake_publication(&nyl_drivers::fake::Script::new());
        #[cfg(all(feature = "test-kinds", not(feature = "tier2")))]
        let drivers = nyl_drivers::registry::with_test_kinds(&nyl_drivers::fake::Script::new());
        #[cfg(not(feature = "test-kinds"))]
        let drivers = nyl_drivers::registry::builtin();
        Self {
            ids: crate::orchestration::ids::UuidV7,
            drivers,
            tools: nyl_drivers::process::ProcessRunner::new().cancelled_by(cancellation.shared()),
            faults: nyl_orchestration::fault::NoFaults,
            cancellation,
            prompt: {
                use std::io::IsTerminal as _;
                std::io::stdin().is_terminal().then(TerminalPrompt::default)
            },
        }
    }
}

/// The terminal of a run waiting on a teardown: shows the message on stderr
/// and takes Enter on stdin as the operator's confirmation. One reader
/// thread, started at the first prompt, feeds the lines it reads to every
/// later prompt; lines typed before a prompt do not confirm it.
#[derive(Default)]
pub struct TerminalPrompt {
    lines: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl OperatorPrompt for TerminalPrompt {
    fn confirm(&self, message: &str, timeout: nyl_core::orchestration::time::Duration) -> bool {
        use std::io::{BufRead as _, Write as _};
        let mut lines = self.lines.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let receiver = lines.get_or_insert_with(|| {
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for line in std::io::stdin().lock().lines() {
                    if line.is_err() || sender.send(()).is_err() {
                        break;
                    }
                }
            });
            receiver
        });
        while receiver.try_recv().is_ok() {}
        let mut stderr = std::io::stderr();
        let _ = writeln!(stderr, "{message} (press Enter to confirm, {timeout} left)");
        let _ = stderr.flush();
        receiver
            .recv_timeout(std::time::Duration::from_secs(timeout.as_secs()))
            .is_ok()
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
            process_env: unicode_env(std::env::vars_os()),
            cancellation: owned.cancellation.clone(),
            prompt: owned.prompt.as_ref().map(|prompt| prompt as &dyn OperatorPrompt),
            observer: None,
            progress: Some(&TerminalProgress),
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

/// The process environment's variables whose name and value are Unicode.
/// Only `PATH` and declared passthrough variables are ever admitted, and
/// those are named in Unicode, so dropping the rest changes nothing a unit
/// can see; collecting with `std::env::vars` would panic on them instead.
fn unicode_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> std::collections::BTreeMap<String, String> {
    vars.into_iter()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

/// Line-oriented live progress; stdout remains available for structured reports.
pub struct TerminalProgress;

impl nyl_core::orchestration::progress::ProgressObserver for TerminalProgress {
    fn observe_worktree_capture(&self, progress: &nyl_core::orchestration::progress::WorktreeCaptureProgress) {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), "{}", worktree_capture_line(progress));
    }

    fn observe(&self, progress: &nyl_core::orchestration::progress::UnitProgress) {
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr().lock(), "{}", progress_line(progress));
    }
}

fn worktree_capture_line(progress: &nyl_core::orchestration::progress::WorktreeCaptureProgress) -> String {
    use nyl_core::orchestration::progress::WorktreeCaptureProgress;
    match progress {
        WorktreeCaptureProgress::Started { base, .. } => format!(
            "[worktree] capturing Git-visible tracked and untracked files (excluding ignored files), base {base}"
        ),
        WorktreeCaptureProgress::Finished {
            snapshot,
            location,
            started,
            at,
        } => {
            let seconds = (*at - *started).to_std().unwrap_or_default().as_secs_f64();
            let action = if snapshot.reused { "reused" } else { "created" };
            format!(
                "[worktree] {action} snapshot {} in {seconds:.1}s\n[worktree] local ref {}\n[worktree] state history {} (ref {}, path {}) when recorded",
                snapshot.commit, snapshot.revision, crate::util::credential_free_url(&location.repository), location.desired_ref,
                if location.path.is_empty() { "/" } else { &location.path }
            )
        }
    }
}

fn progress_line(progress: &nyl_core::orchestration::progress::UnitProgress) -> String {
    use nyl_core::orchestration::progress::UnitPhase;
    match &progress.phase {
        UnitPhase::Started { reason, .. } => format!(
            "[{}] starting {} ({}){}",
            progress.unit,
            progress.kind,
            progress.operation.as_str(),
            reason.map_or_else(String::new, |reason| format!(": {}", reason.description()))
        ),
        UnitPhase::Finished {
            started,
            at,
            result,
            message,
        } => {
            let seconds = (*at - *started).to_std().unwrap_or_default().as_secs_f64();
            let reason = message.as_deref().or(match result {
                nyl_core::orchestration::progress::UnitResult::Blocked { reason }
                | nyl_core::orchestration::progress::UnitResult::Uncertain { reason } => Some(reason.as_str()),
                _ => None,
            });
            let reason = reason
                .and_then(|text| text.lines().next())
                .map(|text| text.chars().take(240).collect::<String>());
            format!(
                "[{}] {} in {seconds:.1}s{}",
                progress.unit,
                result.name(),
                reason
                    .filter(|text| !text.is_empty())
                    .map_or_else(String::new, |text| format!(": {text}"))
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn test_worktree_capture_progress_describes_scope_and_retained_snapshot() {
        use nyl_core::orchestration::progress::WorktreeCaptureProgress;
        let base = nyl_core::orchestration::ids::CommitId::new("1111111111111111111111111111111111111111").unwrap();
        let commit = nyl_core::orchestration::ids::CommitId::new("2222222222222222222222222222222222222222").unwrap();
        let started = "2026-10-28T12:00:00Z".parse().unwrap();
        let at = "2026-10-28T12:00:02Z".parse().unwrap();
        let start = worktree_capture_line(&WorktreeCaptureProgress::Started {
            base: base.clone(),
            at: started,
        });
        assert!(start.contains(base.as_str()));
        assert!(start.contains("tracked and untracked files (excluding ignored files)"));
        for reused in [false, true] {
            let line = worktree_capture_line(&WorktreeCaptureProgress::Finished {
                snapshot: nyl_core::orchestration::ports::WorktreeSnapshot {
                    base: base.clone(),
                    commit: commit.clone(),
                    revision: "refs/nyl-worktrees/test".to_owned(),
                    reused,
                },
                location: nyl_core::orchestration::state::StateLocation {
                    repository: "https://user:secret@example.com/state.git".to_owned(),
                    desired_ref: "refs/heads/state".to_owned(),
                    observed_ref: "refs/heads/observed".to_owned(),
                    path: "dev".to_owned(),
                    coordination_ref_prefix: "refs/nyl/".to_owned(),
                },
                started,
                at,
            });
            assert!(line.contains(if reused { "reused" } else { "created" }));
            assert!(line.contains(commit.as_str()));
            assert!(line.contains("2.0s"));
            assert!(line.contains("refs/nyl-worktrees/test"));
            assert!(line.contains("https://example.com/state.git"));
            assert!(!line.contains("secret"));
        }
    }

    /// Catches a non-Unicode variable unrelated to any unit aborting every
    /// orchestration command.
    #[cfg(unix)]
    #[test]
    fn test_unicode_env_drops_non_unicode_variables() {
        use std::os::unix::ffi::OsStringExt;
        let env = unicode_env([
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("BROKEN"), OsString::from_vec(vec![0xff, 0xfe])),
            (OsString::from_vec(vec![0xff]), OsString::from("x")),
        ]);
        assert_eq!(
            env,
            std::collections::BTreeMap::from([("PATH".to_owned(), "/bin".to_owned())])
        );
    }
}
