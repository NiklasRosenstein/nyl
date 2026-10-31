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
//! detaches its readers. Windows has no process groups to signal: the
//! console delivers Ctrl+C to the tool itself, and the runner kills the
//! tool once the grace passes. Output goes to the
//! transcript sink (stderr by default) with secret values masked. Unit logs
//! can also be appended to files; stdout is returned for steps Nyl reads, such as a
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
const MAX_DIAGNOSTIC_BYTES: usize = 1024 * 1024;

#[cfg(unix)]
mod terminal;

/// Where transcripts go: this process's stderr, or a buffer in tests.
type Sink = Option<Arc<Mutex<Vec<u8>>>>;

#[derive(Debug, Clone, Default)]
struct UnitOutput {
    quiet: bool,
    directory: Option<std::path::PathBuf>,
    file: Option<Arc<Mutex<std::fs::File>>>,
    error: Arc<Mutex<Option<String>>>,
}

/// Runs tools as child processes.
#[derive(Debug, Clone)]
pub struct ProcessRunner {
    /// Time between SIGTERM and SIGKILL; [`KILL_GRACE`] except in tests.
    pub(crate) kill_grace: StdDuration,
    /// A buffer that receives the transcript instead of stderr, for tests.
    pub(crate) transcript: Sink,
    /// Unit address for each public transcript line.
    unit: Option<nyl_core::orchestration::ids::UnitAddress>,
    output: UnitOutput,
    /// The run's cancellation: 0 while it runs, else the signal (such as
    /// SIGINT or SIGTERM) that cancelled it, which the runner forwards to
    /// every running tool's process group, with SIGKILL after the grace.
    cancel: Option<Arc<AtomicI32>>,
}

impl Default for ProcessRunner {
    fn default() -> Self {
        Self {
            kill_grace: KILL_GRACE,
            transcript: None,
            unit: None,
            output: UnitOutput::default(),
            cancel: None,
        }
    }
}

impl ProcessRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Control public unit transcripts. Files append masked lines at
    /// `<directory>/<environment>/<unit>.log`; private stdout is never logged.
    #[must_use]
    pub fn unit_output(mut self, quiet: bool, directory: Option<std::path::PathBuf>) -> Self {
        self.output.quiet = quiet;
        self.output.directory = directory;
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
}

/// How often a spawn that hits `ETXTBSY` is tried again.
const BUSY_ATTEMPTS: u32 = 20;

/// Spawn `command`, trying again while its program is busy.
///
/// Linux refuses to exec a file that is open for writing (`ETXTBSY`). A
/// file just written by this process (a checked-out script) can stay open
/// for writing in a child that another thread forks at that moment, until
/// that child execs and closes it. The window is short, so the spawn is
/// tried again with a growing pause instead of failing the unit.
fn spawn(command: &mut Command) -> std::io::Result<std::process::Child> {
    let mut attempt = 0;
    loop {
        match command.spawn() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt + 1 < BUSY_ATTEMPTS => {
                attempt += 1;
                thread::sleep(StdDuration::from_millis(5 * u64::from(attempt)));
            }
            result => return result,
        }
    }
}

impl ToolRunner for ProcessRunner {
    fn run_for_unit(
        &self,
        invocation: ToolInvocation,
        unit: &nyl_core::orchestration::ids::UnitAddress,
    ) -> DriverResult<ToolExit> {
        let mut runner = self.clone();
        runner.unit = Some(unit.clone());
        runner.output.error = Arc::new(Mutex::new(None));
        if let Some(directory) = &self.output.directory {
            let open = || -> std::io::Result<std::fs::File> {
                let directory = directory.join(unit.environment.as_str());
                std::fs::create_dir_all(&directory)?;
                let mut options = std::fs::OpenOptions::new();
                options.create(true).append(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;
                    options.mode(0o600);
                }
                options.open(directory.join(format!("{}.log", unit.unit)))
            };
            match open() {
                Ok(file) => runner.output.file = Some(Arc::new(Mutex::new(file))),
                Err(error) => warn_log(
                    &runner.transcript,
                    &runner.output,
                    format!("cannot open unit log for {unit}: {error}"),
                ),
            }
        }
        runner.run(invocation)
    }

    #[allow(clippy::too_many_lines)] // One subprocess lifetime owns its streams, terminal, cancellation, deadline, and captured result.
    fn run(&self, invocation: ToolInvocation) -> DriverResult<ToolExit> {
        let mut masked = invocation
            .masked
            .iter()
            .filter(|value| !value.is_empty())
            .cloned()
            .collect::<Vec<_>>();
        // Longest first, so a secret containing another is masked whole.
        masked.sort_by_key(|value| std::cmp::Reverse(value.len()));
        masked.dedup();
        let masked = Arc::new(masked);
        let timeout = invocation.timeout;
        if invocation.private_stdout && (invocation.interactive || invocation.forward_stdout) {
            return Err(DriverError::before_effects(
                "private tool stdout cannot also be forwarded to the terminal",
            ));
        }
        #[cfg(unix)]
        let mut foreground = terminal::Foreground::prepare(invocation.interactive)
            .map_err(|error| DriverError::before_effects(format!("cannot prepare operator terminal: {error}")))?;

        let mut command = Command::new(&invocation.program);
        command
            .args(&invocation.args)
            .current_dir(&invocation.working_dir)
            .env_clear()
            .envs(&invocation.env)
            .stdin(if invocation.interactive {
                Stdio::inherit()
            } else if invocation.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(if invocation.interactive || invocation.forward_stdout {
                Stdio::inherit()
            } else {
                Stdio::piped()
            })
            .stderr(if invocation.interactive {
                Stdio::inherit()
            } else {
                Stdio::piped()
            });
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
        let mut child = match spawn(&mut command) {
            Ok(child) => child,
            Err(error) => {
                return Ok(ToolExit::NotStarted {
                    reason: format!("{}: {error}", invocation.program.display()),
                })
            }
        };
        #[cfg(unix)]
        if let Some(foreground) = foreground.as_mut() {
            if let Err(error) = foreground.activate(child.id()) {
                kill_tool(&mut child);
                let _ = child.wait();
                return Err(DriverError::after_effects(format!(
                    "cannot give operator tool the foreground terminal: {error}"
                )));
            }
        }
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
            let private = invocation.private_stdout;
            let unit = self.unit.clone();
            let output = self.output.clone();
            thread::spawn(move || {
                let captured = if private {
                    let mut captured = Vec::new();
                    let _ = BufReader::new(pipe).read_to_end(&mut captured);
                    captured
                } else {
                    transcribe_for_unit(pipe, &masked, &sink, usize::MAX, unit.as_ref(), &output)
                };
                let _ = done.send(());
                captured
            })
        });
        let stderr = child.stderr.take().map(|pipe| {
            let unit = self.unit.clone();
            let output = self.output.clone();
            let (masked, sink, done) = (masked.clone(), self.transcript.clone(), done.clone());
            thread::spawn(move || {
                let captured = transcribe_for_unit(pipe, &masked, &sink, MAX_DIAGNOSTIC_BYTES, unit.as_ref(), &output);
                let _ = done.send(());
                captured
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
            self.forward_cancellation(&mut child, &mut forwarded);
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
        let diagnostics = stderr
            .map(|reader| reader.join().unwrap_or_default())
            .unwrap_or_default();
        Ok(exit_of(status, captured, diagnostics))
    }
}

impl ProcessRunner {
    /// Forward the cancelling signal to the tool's group once, and SIGKILL
    /// the group when the grace after it has passed. `forwarded` holds the
    /// end of that grace once the signal went out.
    fn forward_cancellation(&self, child: &mut Child, forwarded: &mut Option<Instant>) {
        if forwarded.is_none() {
            if let Some(signal) = self.cancelled() {
                kill_group(child, Signal::Forward(signal));
                *forwarded = Some(Instant::now() + self.kill_grace);
            }
        }
        if forwarded.is_some_and(|end| Instant::now() >= end) {
            kill_tool(child);
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
fn exit_of(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> ToolExit {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => ToolExit::Exited { code, stdout, stderr },
        (None, Some(signal)) => ToolExit::Signaled { signal },
        (None, None) => ToolExit::Exited {
            code: -1,
            stdout,
            stderr,
        },
    }
}

#[cfg(not(unix))]
fn exit_of(status: ExitStatus, stdout: Vec<u8>, stderr: Vec<u8>) -> ToolExit {
    ToolExit::Exited {
        code: status.code().unwrap_or(-1),
        stdout,
        stderr,
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

/// Kill the tool once the cancellation grace has passed: its whole group on
/// unix, the tool itself elsewhere.
fn kill_tool(child: &mut Child) {
    #[cfg(unix)]
    kill_group(child, Signal::Kill);
    #[cfg(not(unix))]
    let _ = child.kill();
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
/// raw bytes up to the capture limit while the full transcript continues.
#[cfg(test)]
fn transcribe(pipe: impl Read, masked: &[String], sink: &Sink, capture_limit: usize) -> Vec<u8> {
    transcribe_for_unit(pipe, masked, sink, capture_limit, None, &UnitOutput::default())
}

fn warn_log(sink: &Sink, output: &UnitOutput, message: String) {
    let mut error = output.error.lock().unwrap();
    if error.is_some() {
        return;
    }
    let warning = format!("warning NYL-UNIT-LOG: {message}; check log directory permissions and free space\n");
    *error = Some(message);
    match sink {
        Some(buffer) => buffer.lock().unwrap().extend_from_slice(warning.as_bytes()),
        None => {
            let _ = std::io::stderr().lock().write_all(warning.as_bytes());
        }
    }
}

fn transcribe_for_unit(
    pipe: impl Read,
    masked: &[String],
    sink: &Sink,
    capture_limit: usize,
    unit: Option<&nyl_core::orchestration::ids::UnitAddress>,
    output: &UnitOutput,
) -> Vec<u8> {
    let mut reader = BufReader::new(pipe);
    let mut captured = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let remaining = capture_limit.saturating_sub(captured.len());
                captured.extend_from_slice(&line[..line.len().min(remaining)]);
                let text = mask(&String::from_utf8_lossy(&line), masked);
                let text = if let Some(unit) = unit {
                    format!("  [{unit}] {text}{}", if text.ends_with('\n') { "" } else { "\n" })
                } else {
                    text
                };
                if let Some(file) = &output.file {
                    if let Err(error) = file.lock().unwrap().write_all(text.as_bytes()) {
                        warn_log(
                            sink,
                            output,
                            format!(
                                "cannot write unit log for {}: {error}",
                                unit.map_or_else(|| "tool".to_owned(), ToString::to_string)
                            ),
                        );
                    }
                }
                if unit.is_some() && output.quiet {
                    continue;
                }
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
            private_stdout: false,
            forward_stdout: false,
            interactive: false,
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

    /// Catches a unit failing with "Text file busy" when its script was
    /// written just before it runs and a concurrent fork still holds the
    /// file open for writing.
    #[cfg(target_os = "linux")]
    #[test]
    fn test_run_retries_a_spawn_while_the_program_is_busy() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::TempDir::new().unwrap();
        let script = directory.path().join("busy.sh");
        let mut writer = std::fs::File::create(&script).unwrap();
        writer.write_all(b"#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // The file stays open for writing until shortly after the spawn starts.
        let closer = thread::spawn(move || {
            thread::sleep(StdDuration::from_millis(30));
            drop(writer);
        });
        let (runner, _) = quiet();
        let exit = runner
            .run(ToolInvocation {
                program: script,
                args: Vec::new(),
                working_dir: directory.path().to_path_buf(),
                env: BTreeMap::new(),
                stdin: None,
                timeout: None,
                private_stdout: false,
                forward_stdout: false,
                interactive: false,
                masked: Vec::new(),
            })
            .unwrap();
        closer.join().unwrap();
        match exit {
            ToolExit::Exited { code, stdout, .. } => {
                assert_eq!(code, 0);
                assert_eq!(String::from_utf8_lossy(&stdout), "ran\n");
            }
            other => panic!("expected the script to run, got {other:?}"),
        }
    }

    #[test]
    fn test_run_captures_stdout_and_exit_code() {
        let (runner, transcript) = quiet();
        let exit = runner.run(sh("echo out; echo err >&2; exit 3")).unwrap();
        assert_eq!(
            exit,
            ToolExit::Exited {
                code: 3,
                stdout: b"out\n".to_vec(),
                stderr: b"err\n".to_vec()
            }
        );
        let transcript = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
        assert!(transcript.contains("out") && transcript.contains("err"), "{transcript}");
    }

    #[test]
    fn test_unit_labels_and_masking_leave_parser_bytes_and_private_stdout_unchanged() {
        use nyl_core::orchestration::ids::UnitAddress;
        let unit: UnitAddress = "dev/web".parse().unwrap();
        let (runner, transcript) = quiet();
        let mut invocation = sh("printf 'secret\nsecond'; printf 'diagnostic-secret' >&2");
        invocation.masked = vec!["secret".to_owned()];
        let exit = runner.run_for_unit(invocation.clone(), &unit).unwrap();
        assert_eq!(
            exit,
            ToolExit::Exited {
                code: 0,
                stdout: b"secret\nsecond".to_vec(),
                stderr: b"diagnostic-secret".to_vec()
            }
        );
        let text = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
        assert!(text.contains("[dev/web] ***\n"), "{text}");
        assert!(text.contains("[dev/web] second\n"), "{text}");
        assert!(text.contains("[dev/web] diagnostic-***\n"), "{text}");
        transcript.lock().unwrap().clear();
        invocation.private_stdout = true;
        assert_eq!(runner.run_for_unit(invocation, &unit).unwrap(), exit);
        assert_eq!(*transcript.lock().unwrap(), b"  [dev/web] diagnostic-***\n");
    }

    #[test]
    fn test_unit_logs_can_tee_or_replace_the_console_without_exposing_private_stdout() {
        let unit = "dev/web".parse().unwrap();
        for quiet_output in [false, true] {
            let logs = tempfile::TempDir::new().unwrap();
            let (runner, transcript) = quiet();
            let runner = runner.unit_output(quiet_output, Some(logs.path().to_owned()));
            let mut invocation = sh("printf 'secret\\n'; printf 'diagnostic\\n' >&2");
            invocation.masked = vec!["secret".to_owned()];
            let ToolExit::Exited { code: 0, stdout, .. } = runner.run_for_unit(invocation, &unit).unwrap() else {
                panic!("tool should succeed")
            };
            assert_eq!(stdout, b"secret\n");
            let mut private = sh("printf 'private-payload'; printf 'more\\n' >&2");
            private.private_stdout = true;
            runner.run_for_unit(private, &unit).unwrap();
            let log = std::fs::read_to_string(logs.path().join("dev/web.log")).unwrap();
            assert!(log.contains("  [dev/web] ***\n"), "{log}");
            assert!(log.contains("  [dev/web] diagnostic\n"), "{log}");
            assert!(log.contains("  [dev/web] more\n"), "{log}");
            assert!(!log.contains("secret") && !log.contains("private-payload"), "{log}");
            let console = String::from_utf8(transcript.lock().unwrap().clone()).unwrap();
            if quiet_output {
                assert!(console.is_empty(), "{console}");
            } else {
                let mut console = console.lines().collect::<Vec<_>>();
                let mut log = log.lines().collect::<Vec<_>>();
                console.sort_unstable();
                log.sort_unstable();
                assert_eq!(console, log);
            }
        }
    }

    #[test]
    fn test_unit_log_setup_failure_warns_and_preserves_tool_exit() {
        let directory = tempfile::TempDir::new().unwrap();
        let logs = directory.path().join("logs");
        std::fs::write(&logs, "not a directory").unwrap();
        let (runner, transcript) = quiet();
        let runner = runner.unit_output(true, Some(logs));
        assert!(matches!(
            runner.run_for_unit(sh("exit 7"), &"dev/web".parse().unwrap()).unwrap(),
            ToolExit::Exited { code: 7, .. }
        ));
        assert!(String::from_utf8_lossy(&transcript.lock().unwrap()).contains("cannot open unit log"));
    }

    #[test]
    #[cfg(unix)]
    fn test_unit_log_write_failure_warns_without_affecting_later_unit() {
        let logs = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(logs.path().join("dev")).unwrap();
        let read_only = logs.path().join("read-only");
        std::fs::write(&read_only, "").unwrap();
        let (mut runner, transcript) = quiet();
        runner.output.quiet = true;
        runner.output.file = Some(Arc::new(Mutex::new(std::fs::File::open(read_only).unwrap())));
        assert!(matches!(
            runner
                .run_for_unit(sh("echo hello; exit 7"), &"dev/web".parse().unwrap())
                .unwrap(),
            ToolExit::Exited { code: 7, .. }
        ));
        runner.output.file = None;
        runner.output.directory = Some(logs.path().to_owned());
        assert!(matches!(
            runner
                .run_for_unit(sh("echo later"), &"dev/other".parse().unwrap())
                .unwrap(),
            ToolExit::Exited { code: 0, .. }
        ));
        assert!(String::from_utf8_lossy(&transcript.lock().unwrap()).contains("cannot write unit log"));
        assert!(std::fs::read_to_string(logs.path().join("dev/other.log"))
            .unwrap()
            .contains("later"));
    }

    #[test]
    fn test_private_stdout_is_captured_without_entering_the_transcript() {
        let (runner, transcript) = quiet();
        let mut invocation = sh("printf 'undeclared-sensitive-output'; printf 'diagnostic' >&2");
        invocation.private_stdout = true;
        let ToolExit::Exited { code: 0, stdout, .. } = runner.run(invocation).unwrap() else {
            panic!("tool did not complete");
        };
        assert_eq!(stdout, b"undeclared-sensitive-output");
        assert_eq!(*transcript.lock().unwrap(), b"diagnostic");
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
                stdout: b"piped".to_vec(),
                stderr: Vec::new()
            }
        );
    }

    #[test]
    fn test_run_environment_is_exactly_the_invocation_env() {
        let (runner, _) = quiet();
        let mut invocation = sh("/usr/bin/env");
        invocation.env = BTreeMap::from([("ONLY".to_owned(), "this".to_owned())]);
        let ToolExit::Exited { code: 0, stdout, .. } = runner.run(invocation).unwrap() else {
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
    fn test_run_masks_invocation_secrets_in_the_transcript() {
        let (runner, transcript) = quiet();
        let mut invocation = sh("echo token=hunter2; echo other=default-secret >&2");
        invocation.masked = vec!["hunter2".to_owned(), "default-secret".to_owned()];
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

    /// Covers the escaped child holding the pipes after the tool exited
    /// and while it still runs.
    #[test]
    fn test_run_deadline_holds_when_an_escaped_child_keeps_the_pipes() {
        // `setsid()` moves the sleeping child out of the tool's process group,
        // so no group signal reaches it, and it holds stdout and stderr open.
        // Perl's POSIX module provides it on Linux and macOS alike, which has
        // no `setsid` command.
        for script in [format!("{ESCAPED_SLEEP} & exit 0"), format!("{ESCAPED_SLEEP} & wait")] {
            let (mut runner, _) = quiet();
            runner.kill_grace = StdDuration::from_millis(200);
            let mut invocation = sh(&script);
            invocation.timeout = Some(StdDuration::from_millis(300));
            let started = Instant::now();
            assert_eq!(runner.run(invocation).unwrap(), ToolExit::DeadlineExceeded, "{script}");
            assert!(
                started.elapsed() < StdDuration::from_secs(5),
                "{script}: {:?}",
                started.elapsed()
            );
        }
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
    #[test]
    fn test_private_stdout_refuses_forwarding_before_starting_a_tool() {
        let (runner, transcript) = quiet();
        let mut invocation = sh("printf 'sensitive JSON'");
        invocation.private_stdout = true;
        invocation.forward_stdout = true;
        let error = runner.run(invocation).unwrap_err();
        assert!(!error.effects_possible());
        assert!(transcript.lock().unwrap().is_empty());
    }
    #[test]
    fn test_stderr_capture_is_bounded_without_truncating_the_transcript() {
        let bytes = b"diagnostic\n".repeat(MAX_DIAGNOSTIC_BYTES / 10 + 100);
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let captured = transcribe(
            std::io::Cursor::new(&bytes),
            &[],
            &Some(transcript.clone()),
            MAX_DIAGNOSTIC_BYTES,
        );
        assert_eq!(captured.len(), MAX_DIAGNOSTIC_BYTES);
        assert_eq!(*transcript.lock().unwrap(), bytes);
    }
}
