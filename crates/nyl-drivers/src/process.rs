//! The process runner: how every driver runs an external tool.
//!
//! Contract: [Drivers](../../../design/orchestration-core.md#drivers). Each
//! tool runs in its own process group with exactly the environment its
//! invocation names. At the deadline, or when the run is cancelled, the
//! group receives SIGTERM and, 30 seconds later, SIGKILL. Output goes to the
//! transcript sink (stderr by default) with secret values masked, and is
//! never stored; stdout is also returned for steps Nyl reads, such as a
//! fingerprint.

use std::time::Duration as StdDuration;

use nyl_core::orchestration::driver::{DriverResult, ToolExit, ToolInvocation, ToolRunner};

/// Time between SIGTERM and SIGKILL.
pub const KILL_GRACE: StdDuration = StdDuration::from_secs(30);

/// Runs tools as child processes.
#[derive(Debug, Clone, Default)]
pub struct ProcessRunner {
    /// Secret values masked in transcripts.
    masked: Vec<String>,
    /// Wall-clock budget per invocation; `None` runs without a deadline.
    /// The shell derives it from the execution's deadline and the injected
    /// clock, so this runner never reads the clock to decide one.
    timeout: Option<StdDuration>,
}

impl ProcessRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mask these values wherever a transcript would show them.
    #[must_use]
    pub fn masking(mut self, secrets: Vec<String>) -> Self {
        self.masked = secrets;
        self
    }

    /// Terminate each invocation's process group after `timeout`.
    #[must_use]
    pub fn with_timeout(mut self, timeout: StdDuration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

impl ToolRunner for ProcessRunner {
    fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit> {
        let _ = (invocation, &self.masked, self.timeout);
        todo!("command-driver")
    }
}
