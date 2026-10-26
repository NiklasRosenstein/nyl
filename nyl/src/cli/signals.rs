//! SIGINT and SIGTERM for the commands that take the lease to change units.
//!
//! Contract: [Runs and leases](../../../design/orchestration-core.md#runs-and-leases),
//! item 7. The first signal sets the command's [`Cancellation`]: a run
//! starts no unit, the process runner forwards the signal to running tools,
//! and the run checkpoints, keeps its run ref, releases the lease, and exits
//! 4 with `NYL-RUN-CANCELLED`; a command waiting for the lease stops
//! waiting. A second signal exits at once with 128 plus its number, as a
//! forced kill would; the next run takes over as after a crash.

use nyl_orchestration::shell::run::{Cancellation, SIGINT, SIGTERM};

/// Set `cancellation` on the first SIGINT or SIGTERM and exit on the second.
/// Runs until the command finishes and aborts it.
pub(super) async fn cancel_on_signal(cancellation: Cancellation) {
    let mut received = Signals::listen();
    while let Some(signal) = received.next().await {
        if cancellation.is_cancelled() {
            std::process::exit(128 + signal);
        }
        cancellation.cancel(signal);
        eprintln!("received signal {signal}: stopping; a second signal exits at once");
    }
}

/// The SIGINT and SIGTERM streams, or nothing where they cannot be watched.
struct Signals {
    #[cfg(unix)]
    streams: Option<(tokio::signal::unix::Signal, tokio::signal::unix::Signal)>,
}

impl Signals {
    fn listen() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let streams = match (signal(SignalKind::interrupt()), signal(SignalKind::terminate())) {
                (Ok(interrupt), Ok(terminate)) => Some((interrupt, terminate)),
                _ => None,
            };
            Self { streams }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// The next signal's number; `None` once no signal can arrive.
    async fn next(&mut self) -> Option<i32> {
        #[cfg(unix)]
        {
            let (interrupt, terminate) = self.streams.as_mut()?;
            tokio::select! {
                received = interrupt.recv() => received.map(|()| SIGINT),
                received = terminate.recv() => received.map(|()| SIGTERM),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = SIGTERM;
            tokio::signal::ctrl_c().await.ok().map(|()| SIGINT)
        }
    }
}
