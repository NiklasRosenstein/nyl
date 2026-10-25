//! Cancelling a run on SIGINT or SIGTERM.
//!
//! Contract: [Runs and leases](../../../../../design/orchestration-core.md#runs-and-leases),
//! item 7. A [`Cancellation`] is a flag the command line's signal handler
//! (or a test) sets with the signal it received. The run shell reads it
//! between phases: once it is set, the run starts no unit, checkpoints the
//! units that finish, gives units that were still running an `uncertain`
//! condition, marks its `Run` record finished, keeps its run ref, releases
//! the lease, and skips the transition commit (`NYL-RUN-CANCELLED`, exit 4).
//! Running tools receive the signal from the process runner, which shares
//! the flag ([`Cancellation::shared`]) and forwards it to each tool's
//! process group, so tools such as OpenTofu stop cleanly and release their
//! own locks.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

/// SIGINT's number, which [`Cancellation::cancel`] callers commonly pass.
pub const SIGINT: i32 = 2;
/// SIGTERM's number.
pub const SIGTERM: i32 = 15;

/// A cloneable cancellation flag holding the signal that cancelled the run;
/// clones share it. The default is never cancelled.
#[derive(Debug, Clone, Default)]
pub struct Cancellation {
    signal: Arc<AtomicI32>,
}

impl Cancellation {
    /// A flag nothing has set yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancel the run with `signal`, such as [`SIGINT`] or [`SIGTERM`]. The
    /// first signal is kept; a non-positive signal is taken as SIGTERM.
    pub fn cancel(&self, signal: i32) {
        let signal = if signal > 0 { signal } else { SIGTERM };
        let _ = self
            .signal
            .compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
    }

    /// Whether the run was cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.signal().is_some()
    }

    /// The signal that cancelled the run.
    pub fn signal(&self) -> Option<i32> {
        match self.signal.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    /// The shared signal cell, for a tool runner that forwards the signal
    /// to running tools (`ProcessRunner::cancelled_by` in `nyl-drivers`):
    /// 0 while the run is not cancelled, else the signal's number.
    pub fn shared(&self) -> Arc<AtomicI32> {
        self.signal.clone()
    }
}

impl PartialEq for Cancellation {
    /// Two flags are equal when they are the same flag.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.signal, &other.signal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a clone that does not see the signal handler's cancel, or a
    /// second signal replacing the first.
    #[test]
    fn test_cancellation_is_shared_by_clones_and_keeps_the_first_signal() {
        let flag = Cancellation::new();
        let clone = flag.clone();
        assert!(!clone.is_cancelled());
        flag.cancel(SIGINT);
        flag.cancel(SIGTERM);
        assert_eq!(clone.signal(), Some(SIGINT));
        assert_eq!(clone.shared().load(Ordering::SeqCst), SIGINT);
    }
}
