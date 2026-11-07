//! Running external programs: every command the suite runs goes through
//! [`Cmd`], which captures stdout and stderr, bounds the run with a timeout,
//! records what it did in the transcript, and, when it fails, reports the
//! command line with the tail of both streams so a CI log is enough to
//! diagnose it.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::{Context, Error, Result};
use crate::log;

/// How many trailing lines of each stream a failure message keeps.
const FAILURE_TAIL_LINES: usize = 120;

/// A command to run: program, arguments, environment, and limits.
#[derive(Clone, Debug)]
pub struct Cmd {
    program: OsString,
    args: Vec<OsString>,
    env: BTreeMap<OsString, OsString>,
    env_clear: bool,
    cwd: Option<PathBuf>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
    echo: bool,
    secrets: Vec<String>,
}

impl Cmd {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            env_clear: false,
            cwd: None,
            stdin: None,
            timeout: Duration::from_secs(300),
            echo: false,
            secrets: Vec::new(),
        }
    }

    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.args.push(argument.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(arguments.into_iter().map(|argument| argument.as_ref().to_owned()));
        self
    }

    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env.insert(name.as_ref().to_owned(), value.as_ref().to_owned());
        self
    }

    pub fn envs<I, K, V>(mut self, variables: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (name, value) in variables {
            self = self.env(name, value);
        }
        self
    }

    /// Start from an empty environment instead of the suite's own.
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    pub fn cwd(mut self, directory: impl AsRef<Path>) -> Self {
        self.cwd = Some(directory.as_ref().to_owned());
        self
    }

    /// Bytes written to the program's standard input.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    /// Kill the program when it runs longer than this (default five minutes).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Show the command, its stderr as it arrives, and its stdout when it ends,
    /// on the console. Quiet commands reach the transcript only, and the
    /// console when they fail.
    pub fn echo(mut self) -> Self {
        self.echo = true;
        self
    }

    /// A value that must never appear in the console, the transcript, or a
    /// failure message.
    pub fn secret(mut self, secret: impl Into<String>) -> Self {
        let secret = secret.into();
        if !secret.is_empty() {
            self.secrets.push(secret);
        }
        self
    }

    /// [`secret`](Self::secret) for each value.
    pub fn secrets_from(mut self, secrets: impl IntoIterator<Item = String>) -> Self {
        for secret in secrets {
            self = self.secret(secret);
        }
        self
    }

    /// The command line as one would type it, with secrets hidden.
    pub fn display(&self) -> String {
        let mut parts = vec![quote(&self.program.to_string_lossy())];
        parts.extend(self.args.iter().map(|argument| quote(&argument.to_string_lossy())));
        let mut text = parts.join(" ");
        if let Some(cwd) = &self.cwd {
            text.push_str(&format!("   (in {})", cwd.display()));
        }
        mask(&self.secrets, &text)
    }

    /// Run to completion. An error is a program that could not start; a
    /// program that ran and failed is an [`Output`] with its exit code.
    pub fn run(&self) -> Result<Output> {
        let display = self.display();
        if self.echo {
            log::line(format!("$ {display}"));
        } else {
            log::transcript_only(&format!("[{}] $ {display}\n", log::elapsed_label()));
        }
        let started = Instant::now();

        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.env_clear {
            command.env_clear();
        }
        command.envs(&self.env);
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        let mut child = command.spawn().context(format!(
            "cannot start `{display}` (is the program installed and on PATH?)"
        ))?;

        let writer = child.stdin.take().map(|mut pipe| {
            let data = self.stdin.clone().unwrap_or_default();
            thread::spawn(move || {
                // The program may exit without reading its input.
                let _ = pipe.write_all(&data);
            })
        });
        let stdout = child.stdout.take().map(|pipe| drain(pipe, false));
        let stderr = child.stderr.take().map(|pipe| drain(pipe, self.echo));

        let mut timed_out = false;
        let status = loop {
            match child.try_wait().context(format!("wait for `{display}`"))? {
                Some(status) => break status,
                None if started.elapsed() >= self.timeout => {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().context(format!("wait for killed `{display}`"))?;
                }
                None => thread::sleep(Duration::from_millis(25)),
            }
        };
        if let Some(writer) = writer {
            let _ = writer.join();
        }
        // A program that was killed may have left children holding its pipes
        // open; what it printed so far is all there will be.
        let stdout = stdout.map(|stream| stream.finish(!timed_out)).unwrap_or_default();
        let stderr = stderr.map(|stream| stream.finish(!timed_out)).unwrap_or_default();

        let output = Output {
            code: status.code(),
            stdout,
            stderr,
            command: display,
            elapsed: started.elapsed(),
            timed_out,
            secrets: self.secrets.clone(),
        };
        output.record(self.echo);
        Ok(output)
    }

    /// [`run`](Self::run), failing unless the exit code is 0.
    pub fn run_ok(&self) -> Result<Output> {
        self.run_expect(&[0])
    }

    /// [`run`](Self::run), failing unless the exit code is one of `codes`.
    pub fn run_expect(&self, codes: &[i32]) -> Result<Output> {
        let output = self.run()?;
        match output.code {
            Some(code) if codes.contains(&code) && !output.timed_out => Ok(output),
            _ => Err(Error::new(output.describe_failure(codes))),
        }
    }
}

/// What a finished program produced.
#[derive(Clone, Debug)]
pub struct Output {
    /// `None` when a signal ended the program.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    /// The command line, secrets hidden.
    pub command: String,
    pub elapsed: Duration,
    pub timed_out: bool,
    secrets: Vec<String>,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    pub fn stdout_trimmed(&self) -> &str {
        self.stdout.trim()
    }

    /// The command line, how it ended, and the tail of both streams.
    pub fn describe_failure(&self, expected: &[i32]) -> String {
        let ending = if self.timed_out {
            "was killed after its timeout".to_owned()
        } else {
            match self.code {
                Some(code) => format!("exited with {code} (expected {expected:?})"),
                None => "was ended by a signal".to_owned(),
            }
        };
        let mut text = format!("`{}` {ending} after {:.1}s", self.command, self.elapsed.as_secs_f64());
        for (name, stream) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
            if !stream.trim().is_empty() {
                let tail = truncate_tail(&mask(&self.secrets, stream), FAILURE_TAIL_LINES);
                text.push_str(&format!("\n--- {name} ---\n{}", tail.trim_end()));
            }
        }
        text
    }

    fn record(&self, echo: bool) {
        let ending = match (self.timed_out, self.code) {
            (true, _) => "timed out".to_owned(),
            (false, Some(code)) => format!("exit {code}"),
            (false, None) => "signal".to_owned(),
        };
        let summary = format!("({ending} in {:.1}s)\n", self.elapsed.as_secs_f64());
        if echo {
            // stderr was already streamed.
            if !self.stdout.trim().is_empty() {
                log::raw(&mask(&self.secrets, &truncate_tail(&self.stdout, FAILURE_TAIL_LINES)));
                if !self.stdout.ends_with('\n') {
                    log::raw("\n");
                }
            }
            log::raw(&summary);
        } else {
            let mut transcript = String::new();
            for (name, stream) in [("stdout", &self.stdout), ("stderr", &self.stderr)] {
                if !stream.is_empty() {
                    transcript.push_str(&format!("--- {name} ---\n{}", mask(&self.secrets, stream)));
                    if !stream.ends_with('\n') {
                        transcript.push('\n');
                    }
                }
            }
            transcript.push_str(&summary);
            log::transcript_only(&transcript);
        }
    }
}

/// A stream being read on a thread of its own.
struct Stream {
    handle: JoinHandle<()>,
    collected: Arc<Mutex<Vec<u8>>>,
}

impl Stream {
    /// Everything read so far; waits for the end of the stream when `wait`.
    fn finish(self, wait: bool) -> String {
        if wait {
            let _ = self.handle.join();
        }
        let bytes = self.collected.lock().map(|bytes| bytes.clone()).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn drain(mut source: impl Read + Send + 'static, tee: bool) -> Stream {
    let collected = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    let handle = thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            match source.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if let Ok(mut bytes) = sink.lock() {
                        bytes.extend_from_slice(&buffer[..count]);
                    }
                    if tee {
                        log::raw(&String::from_utf8_lossy(&buffer[..count]));
                    }
                }
            }
        }
    });
    Stream { handle, collected }
}

/// Replace every secret in `text` with `***`.
pub fn mask(secrets: &[String], text: &str) -> String {
    let mut masked = text.to_owned();
    for secret in secrets {
        masked = masked.replace(secret.as_str(), "***");
    }
    masked
}

/// The last `lines` lines of `text`, with a note when earlier ones were cut.
pub fn truncate_tail(text: &str, lines: usize) -> String {
    let all = text.lines().collect::<Vec<_>>();
    if all.len() <= lines {
        return text.to_owned();
    }
    let skipped = all.len() - lines;
    format!("... ({skipped} earlier lines omitted)\n{}\n", all[skipped..].join("\n"))
}

/// Quote an argument for display when it holds anything a shell would split.
fn quote(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument
            .chars()
            .all(|character| character.is_alphanumeric() || "-_./:=@%+,".contains(character));
    if plain {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_hides_every_occurrence_of_each_secret() {
        let secrets = vec!["token-1".to_owned(), "hunter2".to_owned()];
        assert_eq!(
            mask(&secrets, "Bearer token-1 and token-1 with hunter2"),
            "Bearer *** and *** with ***"
        );
    }

    #[test]
    fn test_truncate_tail_keeps_the_end_and_says_what_it_cut() {
        let text = (1..=10).map(|n| n.to_string()).collect::<Vec<_>>().join("\n");
        assert_eq!(truncate_tail(&text, 20), text);
        let cut = truncate_tail(&text, 3);
        assert!(cut.starts_with("... (7 earlier lines omitted)\n8\n9\n10"), "{cut}");
    }

    #[test]
    fn test_display_quotes_arguments_a_shell_would_split_and_hides_secrets() {
        let command = Cmd::new("curl")
            .args(["-H", "Authorization: Bearer abc", "http://127.0.0.1:1/x"])
            .secret("abc");
        assert_eq!(
            command.display(),
            "curl -H 'Authorization: Bearer ***' http://127.0.0.1:1/x"
        );
    }

    #[test]
    fn test_a_program_that_cannot_start_is_an_error_naming_the_command() {
        let error = Cmd::new("nyl-e2e-no-such-program").arg("--flag").run().unwrap_err();
        assert!(error.to_string().contains("nyl-e2e-no-such-program --flag"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn test_run_captures_both_streams_and_the_exit_code() {
        let output = Cmd::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .run()
            .unwrap();
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout_trimmed(), "out");
        assert_eq!(output.stderr.trim(), "err");
        let error = Cmd::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .run_ok()
            .unwrap_err()
            .to_string();
        assert!(error.contains("exited with 3 (expected [0])"), "{error}");
        assert!(error.contains("--- stdout ---\nout"), "{error}");
        assert!(error.contains("--- stderr ---\nerr"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn test_run_kills_a_program_that_outlives_its_timeout() {
        let output = Cmd::new("sh")
            .args(["-c", "sleep 30"])
            .timeout(Duration::from_millis(200))
            .run()
            .unwrap();
        assert!(output.timed_out);
        assert!(!output.success());
        assert!(output.elapsed < Duration::from_secs(20));
    }

    #[cfg(unix)]
    #[test]
    fn test_run_passes_stdin_and_only_the_given_environment() {
        let output = Cmd::new("sh")
            .args(["-c", "cat; printf %s \"$E2E_ONLY\"; printf %s \"${HOME-unset}\""])
            .env_clear()
            .env("E2E_ONLY", "x")
            .stdin("input-")
            .run_ok()
            .unwrap();
        assert_eq!(output.stdout, "input-xunset");
    }
}
