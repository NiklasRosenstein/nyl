//! The process runner: how every driver runs an external tool.
//!
//! Contract: [Drivers](../../../design/orchestration-core.md#drivers). Each
//! tool runs in its own process group with exactly the environment its
//! invocation names. At the deadline the group receives SIGTERM and, 30
//! seconds later, SIGKILL; when the run is cancelled
//! ([`ProcessRunner::cancelled_by`]), the group receives the run's own
//! signal, such as SIGINT, and SIGKILL after the same grace, and no further
//! tool starts. This holds
//! whether the tool is still running or has exited while other members of
//! its group, or processes that left it, keep its output open: the runner
//! waits for output no longer than the deadline plus the grace, then
//! detaches its readers. Output goes to the
//! transcript sink (stderr by default) with secret values masked, and is
//! never stored; stdout is also returned for steps Nyl reads, such as a
//! fingerprint.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use nyl_core::orchestration::driver::{DriverError, DriverResult, ToolExit, ToolInvocation, ToolRunner};

/// Time between SIGTERM and SIGKILL.
pub const KILL_GRACE: StdDuration = StdDuration::from_secs(30);

/// How often the runner checks whether a tool has exited.
const POLL_INTERVAL: StdDuration = StdDuration::from_millis(10);

/// What replaces a masked value in the transcript.
const MASK: &str = "***";

/// Where transcripts go: this process's stderr, or a buffer in tests.
type Sink = Option<Arc<Mutex<Vec<u8>>>>;

/// Runs tools as child processes.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    /// Secret values masked in every transcript, in addition to each
    /// invocation's own [`ToolInvocation::masked`].
    masked: Vec<String>,
    /// Default budget for invocations without [`ToolInvocation::timeout`];
    /// `None` runs those without a deadline. Drivers set the per-invocation
    /// timeout from their context's `deadline - now`, so this runner never
    /// reads a clock to decide one (it measures elapsed time with `Instant`).
    timeout: Option<StdDuration>,
    /// Time between SIGTERM and SIGKILL; [`KILL_GRACE`] except in tests.
    pub(crate) kill_grace: StdDuration,
    /// A buffer that receives the transcript instead of stderr, for tests.
    pub(crate) transcript: Sink,
    /// The run's cancellation: 0 while it runs, else the signal (such as
    /// SIGINT or SIGTERM) that cancelled it, which the runner forwards to
    /// every running tool's process group, with SIGKILL after the grace.
    cancel: Option<Arc<AtomicI32>>,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            masked: Vec::new(),
            timeout: None,
            kill_grace: KILL_GRACE,
            transcript: None,
            cancel: None,
        }
    }
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

    /// Forward the signal `cancel` holds, once it is set (non-zero), to the
    /// process group of every running tool, and start no tool afterwards.
    /// A tool that has not exited when [`Self::kill_grace`] passes after
    /// the signal is killed. The run shell's `Cancellation::shared` is
    /// such a cell. Contract: [Runs and leases](../../../design/orchestration-core.md#runs-and-leases), item 7.
    #[must_use]
    pub fn cancelled_by(mut self, cancel: Arc<AtomicI32>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// The signal that cancelled the run, if it was.
    fn cancelled(&self) -> Option<i32> {
        self.cancel
            .as_ref()
            .map(|cancel| cancel.load(Ordering::SeqCst))
            .filter(|signal| *signal != 0)
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
        let mut masked = self
            .masked
            .iter()
            .chain(&invocation.masked)
            .filter(|value| !value.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        // Longest first, so a secret containing another is masked whole.
        masked.sort_by_key(|value| std::cmp::Reverse(value.len()));
        masked.dedup();
        let masked = Arc::new(masked);
        let timeout = invocation.timeout.or(self.timeout);

        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .current_dir(&invocation.working_dir)
            .env_clear()
            .envs(&invocation.env)
            .stdin(if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        if let Some(signal) = self.cancelled() {
            return Ok(ToolExit::NotStarted {
                reason: format!("the run was cancelled by signal {signal}"),
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolExit::NotStarted {
                    reason: format!("{}: {error}", invocation.program.display()),
                })
            }
        };
        let start = Instant::now();
        let deadline = timeout.map(|timeout| start + timeout);

        if let (Some(bytes), Some(mut stdin)) = (invocation.stdin, child.stdin.take()) {
            // A tool that never reads its input must not block the runner.
            thread::spawn(move || {
                let _ = stdin.write_all(&bytes);
            });
        }
        let (done, finished) = mpsc::channel::<()>();
        let stdout = child.stdout.take().map(|pipe| {
            let (masked, sink, done) = (masked.clone(), self.transcript.clone(), done.clone());
            thread::spawn(move || {
                let captured = transcribe(pipe, &masked, &sink, true);
                let _ = done.send(());
                captured
            })
        });
        let stderr = child.stderr.take().map(|pipe| {
            let (masked, sink, done) = (masked.clone(), self.transcript.clone(), done.clone());
            thread::spawn(move || {
                transcribe(pipe, &masked, &sink, false);
                let _ = done.send(());
            })
        });
        drop(done);
        let mut open = usize::from(stdout.is_some()) + usize::from(stderr.is_some());

        // When the run is cancelled, the signal goes to the group once, and
        // the tool gets the kill grace to stop cleanly.
        let mut forwarded: Option<Instant> = None;
        let status = loop {
            if let Some(status) = child.try_wait().map_err(wait_error)? {
                break status;
            }
            self.forward_cancellation(&child, &mut forwarded);
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                // Readers still blocked on a pipe that a process outside the
                // group holds open are detached, never joined.
                self.terminate(&mut child, false, &finished, &mut open)?;
                return Ok(ToolExit::DeadlineExceeded);
            }
            thread::sleep(POLL_INTERVAL);
        };

        // The tool exited; its output ends when every process holding its
        // pipes, such as a background child, has exited too. That wait is
        // bounded by the deadline like the tool itself and, once the run is
        // cancelled, by the kill grace after the forwarded signal.
        match self.drain_after_exit(&child, &finished, &mut open, deadline, &mut forwarded) {
            Drained::Finished => {}
            Drained::DeadlineExceeded => {
                self.terminate(&mut child, true, &finished, &mut open)?;
                return Ok(ToolExit::DeadlineExceeded);
            }
            Drained::Cancelled => return Ok(ToolExit::Cancelled),
        }
        let captured = stdout
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default();
        if let Some(reader) = stderr {
            let _ = reader.join();
        }
        Ok(exit_of(status, captured))
    }
}

impl ProcessRunner {
    /// Forward the cancelling signal to the tool's group once, and SIGKILL
    /// the group when the grace after it has passed. `forwarded` holds the
    /// end of that grace once the signal went out.
    fn forward_cancellation(&self, child: &Child, forwarded: &mut Option<Instant>) {
        if forwarded.is_none() {
            if let Some(signal) = self.cancelled() {
                kill_group(child, Signal::Forward(signal));
                *forwarded = Some(Instant::now() + self.kill_grace);
            }
        }
        if forwarded.is_some_and(|end| Instant::now() >= end) {
            kill_group(child, Signal::Kill);
        }
    }
}

/// How waiting for a tool's output after the tool exited ended.
enum Drained {
    /// Every process holding the tool's pipes has closed them.
    Finished,
    /// The deadline passed first.
    DeadlineExceeded,
    /// The run was cancelled and the grace after the forwarded signal
    /// passed; the group was killed and the readers detached.
    Cancelled,
}

impl ProcessRunner {
    /// Wait for the transcript of a tool whose leader exited while members
    /// of its group, or processes that left it, may keep its pipes open.
    /// The run's cancellation is still honored: the signal goes to the group
    /// (once, unless it went out while the leader ran), and the group is
    /// killed and the readers detached when the grace after it passes.
    fn drain_after_exit(
        &self,
        child: &Child,
        finished: &mpsc::Receiver<()>,
        open: &mut usize,
        deadline: Option<Instant>,
        forwarded: &mut Option<Instant>,
    ) -> Drained {
        while *open > 0 {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Drained::DeadlineExceeded;
            }
            if forwarded.is_none() {
                if let Some(signal) = self.cancelled() {
                    kill_group(child, Signal::Forward(signal));
                    *forwarded = Some(Instant::now() + self.kill_grace);
                }
            }
            if forwarded.is_some_and(|end| Instant::now() >= end) {
                kill_group(child, Signal::Kill);
                drain(finished, open, Instant::now() + KILLED_DRAIN);
                return Drained::Cancelled;
            }
            let mut wait = POLL_INTERVAL;
            if let Some(until) = deadline.into_iter().chain(*forwarded).min() {
                wait = wait.min(until.saturating_duration_since(Instant::now()));
            }
            match finished.recv_timeout(wait) {
                Ok(()) => *open -= 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                // Every reader dropped its sender, so every reader finished.
                Err(mpsc::RecvTimeoutError::Disconnected) => *open = 0,
            }
        }
        Drained::Finished
    }
}

/// How long the runner still waits for the transcript once the group was
/// killed. A process that left the group can hold the tool's pipes open
/// indefinitely; its readers are detached when this passes.
const KILLED_DRAIN: StdDuration = StdDuration::from_millis(500);

impl ProcessRunner {
    /// Stop the tool at its deadline: SIGTERM to the group, SIGKILL once
    /// [`Self::kill_grace`] passes while any member is alive. Returns once
    /// the tool itself is reaped and its transcript drained, or once the
    /// grace (plus [`KILLED_DRAIN`] after SIGKILL) has passed, whichever
    /// comes first; it never waits on pipes without a bound.
    #[cfg(not(unix))]
    fn terminate(
        &self,
        child: &mut Child,
        leader_exited: bool,
        finished: &mpsc::Receiver<()>,
        open: &mut usize,
    ) -> DriverResult<()> {
        if !leader_exited {
            let _ = child.kill();
            child.wait().map_err(wait_error)?;
        }
        drain(finished, open, Instant::now() + KILLED_DRAIN.min(self.kill_grace));
        Ok(())
    }

    #[cfg(unix)]
    fn terminate(
        &self,
        child: &mut Child,
        mut leader_exited: bool,
        finished: &mpsc::Receiver<()>,
        open: &mut usize,
    ) -> DriverResult<()> {
        kill_group(child, Signal::Term);
        let grace_end = Instant::now() + self.kill_grace;
        while Instant::now() < grace_end {
            if !leader_exited {
                leader_exited = child.try_wait().map_err(wait_error)?.is_some();
            }
            while *open > 0 && finished.try_recv().is_ok() {
                *open -= 1;
            }
            if leader_exited && !group_alive(child) {
                // The group is gone; what still holds the pipes, if anything,
                // left the group. Wait for it only for the rest of the grace.
                drain(finished, open, grace_end);
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
        kill_group(child, Signal::Kill);
        if !leader_exited {
            child.wait().map_err(wait_error)?;
        }
        drain(finished, open, Instant::now() + KILLED_DRAIN);
        Ok(())
    }
}

/// Count finished transcript readers until none is `open` or `until`
/// passes. Returns whether every reader finished.
fn drain(finished: &mpsc::Receiver<()>, open: &mut usize, until: Instant) -> bool {
    while *open > 0 {
        let remaining = until.saturating_duration_since(Instant::now());
        match finished.recv_timeout(remaining) {
            Ok(()) => *open -= 1,
            Err(mpsc::RecvTimeoutError::Timeout) => return false,
            // Every reader dropped its sender, so every reader finished.
            Err(mpsc::RecvTimeoutError::Disconnected) => *open = 0,
        }
    }
    true
}

#[allow(clippy::needless_pass_by_value)] // Used as `map_err(wait_error)`.
fn wait_error(error: std::io::Error) -> DriverError {
    DriverError::after_effects(format!("failed to wait for a tool: {error}"))
}

#[cfg(unix)]
fn exit_of(status: ExitStatus, stdout: Vec<u8>) -> ToolExit {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => ToolExit::Exited { code, stdout },
        (None, Some(signal)) => ToolExit::Signaled { signal },
        (None, None) => ToolExit::Exited { code: -1, stdout },
    }
}

#[cfg(not(unix))]
fn exit_of(status: ExitStatus, stdout: Vec<u8>) -> ToolExit {
    ToolExit::Exited {
        code: status.code().unwrap_or(-1),
        stdout,
    }
}

#[derive(Debug, Clone, Copy)]
#[cfg_attr(not(unix), allow(dead_code))] // Only unix has process groups to signal.
enum Signal {
    Term,
    Kill,
    /// A signal the run received, forwarded on cancellation.
    Forward(i32),
}

/// Send a signal to the tool's process group, which `process_group(0)` made
/// its own: the group id is the tool's pid.
#[cfg(unix)]
fn kill_group(child: &Child, signal: Signal) {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        return;
    };
    let signal = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
        Signal::Forward(signal) => signal,
    };
    // SAFETY: kill(2) has no memory-safety preconditions; a group that no
    // longer exists yields ESRCH, which is ignored.
    unsafe {
        libc::kill(-pgid, signal);
    }
}

/// Without process groups, [`ProcessRunner::terminate`] stops the tool
/// itself, and stray children of an exited tool are left alone.
#[cfg(not(unix))]
fn kill_group(_child: &Child, _signal: Signal) {}

/// Whether any process of the tool's group is still alive.
#[cfg(unix)]
fn group_alive(child: &Child) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(child.id()) else {
        return false;
    };
    // SAFETY: signal 0 only checks for existence.
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Copy a tool's output, line by line and masked, to the transcript; return
/// the raw bytes when `capture` is set.
fn transcribe(pipe: impl Read, masked: &[String], sink: &Sink, capture: bool) -> Vec<u8> {
    let mut reader = BufReader::new(pipe);
    let mut captured = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if capture {
                    captured.extend_from_slice(&line);
                }
                let text = mask(&String::from_utf8_lossy(&line), masked);
                match sink {
                    Some(buffer) => buffer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(text.as_bytes()),
                    None => {
                        let _ = std::io::stderr().lock().write_all(text.as_bytes());
                    }
                }
            }
        }
    }
    captured
}

/// Replace every masked value in `text` with `***`.
pub(crate) fn mask(text: &str, masked: &[String]) -> String {
    let mut text = text.to_owned();
    for value in masked {
        if !value.is_empty() && text.contains(value.as_str()) {
            text = text.replace(value.as_str(), MASK);
        }
    }
    text
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;

    fn sh(script: &str) -> ToolInvocation {
        ToolInvocation {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            working_dir: std::env::temp_dir(),
            env: BTreeMap::new(),
            stdin: None,
            timeout: None,
            masked: Vec::new(),
        }
    }

    fn quiet() -> (ProcessRunner, Arc<Mutex<Vec<u8>>>) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let runner = ProcessRunner {
            transcript: Some(buffer.clone()),
            ..ProcessRunner::new()
        };
        (runner, buffer)
    }

    #[test]
    fn test_run_captures_stdout_and_exit_code() {
        let (runner, transcript) = quiet();
        let exit = runner.run(sh("echo out; echo err >&2; exit 3")).unwrap();
        assert_eq!(
            exit,
            ToolExit::Exited {
                code: 3,
                stdout: b"out\n".to_vec()
            }
        );
        let transcript = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
        assert!(transcript.contains("out") && transcript.contains("err"), "{transcript}");
    }

    #[test]
    fn test_run_passes_stdin_bytes() {
        let (runner, _) = quiet();
        let mut invocation = sh("cat");
        invocation.stdin = Some(b"piped".to_vec());
        let exit = runner.run(invocation).unwrap();
        assert_eq!(
            exit,
            ToolExit::Exited {
                code: 0,
                stdout: b"piped".to_vec()
            }
        );
    }

    #[test]
    fn test_run_environment_is_exactly_the_invocation_env() {
        let (runner, _) = quiet();
        let mut invocation = sh("/usr/bin/env");
        invocation.env = BTreeMap::from([("ONLY".to_owned(), "this".to_owned())]);
        let ToolExit::Exited { code: 0, stdout } = runner.run(invocation).unwrap() else {
            panic!("env runs");
        };
        let mut lines = String::from_utf8(stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        // `sh` itself may export PWD, SHLVL and `_`; nothing else of ours leaks.
        lines.retain(|line| !line.starts_with("PWD=") && !line.starts_with("SHLVL=") && !line.starts_with("_="));
        assert_eq!(lines, vec!["ONLY=this".to_owned()]);
    }

    #[test]
    fn test_run_masks_invocation_and_default_secrets_in_the_transcript() {
        let (runner, transcript) = quiet();
        let runner = runner.masking(vec!["default-secret".to_owned()]);
        let mut invocation = sh("echo token=hunter2; echo other=default-secret >&2");
        invocation.masked = vec!["hunter2".to_owned()];
        let ToolExit::Exited { stdout, .. } = runner.run(invocation).unwrap() else {
            panic!("the tool runs");
        };
        // Captured stdout is the tool's own bytes; only the transcript is masked.
        assert_eq!(stdout, b"token=hunter2\n");
        let transcript = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
        assert!(transcript.contains("token=***"), "{transcript}");
        assert!(transcript.contains("other=***"), "{transcript}");
        assert!(!transcript.contains("hunter2") && !transcript.contains("default-secret"));
    }

    #[test]
    fn test_run_timeout_kills_the_whole_process_group() {
        let (mut runner, _) = quiet();
        runner.kill_grace = StdDuration::from_millis(200);
        let mut invocation = sh("sleep 30 & wait");
        invocation.timeout = Some(StdDuration::from_millis(200));
        let started = Instant::now();
        assert_eq!(runner.run(invocation).unwrap(), ToolExit::DeadlineExceeded);
        // The background `sleep` holds the stdout pipe, and the runner returns
        // only once that pipe closes: the whole group was terminated.
        assert!(started.elapsed() < StdDuration::from_secs(10));
    }

    /// A 30-second sleep in a new session, outside the tool's process group.
    const ESCAPED_SLEEP: &str = "perl -MPOSIX -e 'POSIX::setsid(); sleep 30'";

    #[test]
    fn test_run_deadline_holds_when_an_escaped_child_keeps_the_pipes_after_the_tool_exits() {
        let (mut runner, _) = quiet();
        runner.kill_grace = StdDuration::from_millis(200);
        // `setsid()` moves the sleeping child out of the tool's process group,
        // so no group signal reaches it, and it holds stdout and stderr open.
        // Perl's POSIX module provides it on Linux and macOS alike, which has
        // no `setsid` command.
        let mut invocation = sh(&format!("{ESCAPED_SLEEP} & exit 0"));
        invocation.timeout = Some(StdDuration::from_millis(300));
        let started = Instant::now();
        assert_eq!(runner.run(invocation).unwrap(), ToolExit::DeadlineExceeded);
        assert!(started.elapsed() < StdDuration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn test_run_deadline_holds_when_an_escaped_child_keeps_the_pipes_while_the_tool_runs() {
        let (mut runner, _) = quiet();
        runner.kill_grace = StdDuration::from_millis(200);
        let mut invocation = sh(&format!("{ESCAPED_SLEEP} & wait"));
        invocation.timeout = Some(StdDuration::from_millis(300));
        let started = Instant::now();
        assert_eq!(runner.run(invocation).unwrap(), ToolExit::DeadlineExceeded);
        assert!(started.elapsed() < StdDuration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn test_run_terminates_a_lingering_group_with_sigterm_after_the_tool_exits() {
        let (mut runner, transcript) = quiet();
        runner.kill_grace = StdDuration::from_secs(5);
        // A background member of the group outlives the tool and handles
        // SIGTERM; SIGKILL would give it no chance to report.
        let mut invocation = sh("(trap 'echo got-term >&2; exit 0' TERM; sleep 30 & wait) & exit 0");
        invocation.timeout = Some(StdDuration::from_millis(300));
        assert_eq!(runner.run(invocation).unwrap(), ToolExit::DeadlineExceeded);
        let transcript = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
        assert!(transcript.contains("got-term"), "{transcript}");
    }

    /// Catches a cancelled run leaving its tools running, which would keep
    /// effects going and native locks held after Nyl exits.
    #[test]
    fn test_run_forwards_the_cancelling_signal_to_the_tool_group() {
        let (runner, transcript) = quiet();
        let cancel = Arc::new(AtomicI32::new(0));
        let runner = runner.cancelled_by(cancel.clone());
        let mut invocation = sh("trap 'echo got-term >&2; exit 7' TERM; sleep 30 & wait");
        invocation.timeout = Some(StdDuration::from_secs(20));
        let signaller = thread::spawn(move || {
            thread::sleep(StdDuration::from_millis(300));
            cancel.store(libc::SIGTERM, Ordering::SeqCst);
        });
        let started = Instant::now();
        let exit = runner.run(invocation).unwrap();
        signaller.join().unwrap();
        assert!(matches!(exit, ToolExit::Exited { code: 7, .. }), "{exit:?}");
        assert!(
            started.elapsed() < StdDuration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let transcript = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
        assert!(transcript.contains("got-term"), "{transcript}");
        // Once cancelled, no further tool starts.
        assert!(matches!(runner.run(sh("true")).unwrap(), ToolExit::NotStarted { .. }));
    }

    /// Catches a cancelled run waiting on output until the unit's deadline
    /// (up to an hour) after the tool's leader exited, while a member of its
    /// group that ignores the signal keeps the pipes open: CI allows only
    /// seconds between the signal and a forced kill. Covers the signal
    /// arriving while the leader runs and after it exited.
    #[test]
    fn test_run_cancellation_kills_a_group_member_holding_the_pipes_after_the_tool_exits() {
        let scripts = [
            // The leader exits on the forwarded signal; the member ignores it.
            "(trap '' TERM; sleep 30) & trap 'exit 7' TERM; while :; do sleep 0.05; done",
            // The leader exits before the signal arrives.
            "(trap '' TERM; sleep 30) & exit 0",
        ];
        for script in scripts {
            let (runner, _) = quiet();
            let cancel = Arc::new(AtomicI32::new(0));
            let runner = ProcessRunner {
                kill_grace: StdDuration::from_millis(300),
                ..runner.cancelled_by(cancel.clone())
            };
            let mut invocation = sh(script);
            invocation.timeout = Some(StdDuration::from_secs(25));
            let signaller = thread::spawn(move || {
                thread::sleep(StdDuration::from_millis(300));
                cancel.store(libc::SIGTERM, Ordering::SeqCst);
            });
            let started = Instant::now();
            let exit = runner.run(invocation).unwrap();
            signaller.join().unwrap();
            assert_eq!(exit, ToolExit::Cancelled, "{script}");
            assert!(
                started.elapsed() < StdDuration::from_secs(10),
                "{script}: {:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn test_run_reports_a_signal_death() {
        let (runner, _) = quiet();
        assert_eq!(
            runner.run(sh("kill -9 $$")).unwrap(),
            ToolExit::Signaled { signal: libc::SIGKILL }
        );
    }

    #[test]
    fn test_run_missing_program_is_not_started() {
        let (runner, _) = quiet();
        let mut invocation = sh("");
        invocation.program = PathBuf::from("/nonexistent/tool");
        assert!(matches!(runner.run(invocation).unwrap(), ToolExit::NotStarted { .. }));
    }
}
