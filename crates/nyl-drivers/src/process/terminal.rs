//! Foreground ownership for an operator tool with inherited terminal streams.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;

/// Restores Nyl's foreground process group even on timeout or cancellation.
pub(super) struct Foreground {
    tty: File,
    previous: libc::pid_t,
    active: bool,
}
impl Foreground {
    pub(super) fn prepare(interactive: bool) -> io::Result<Option<Self>> {
        if !interactive {
            return Ok(None);
        }
        let tty = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        // SAFETY: the open descriptor is a controlling terminal; these calls only inspect process IDs.
        let previous = unsafe { libc::tcgetpgrp(tty.as_raw_fd()) };
        if previous < 0 {
            return Err(io::Error::last_os_error());
        }
        if previous != unsafe { libc::getpgrp() } {
            return Err(io::Error::other(
                "interactive execution requires Nyl to own the foreground terminal",
            ));
        }
        Ok(Some(Self {
            tty,
            previous,
            active: false,
        }))
    }
    pub(super) fn activate(&mut self, pid: u32) -> io::Result<()> {
        let group = libc::pid_t::try_from(pid).map_err(|_| io::Error::other("invalid tool process group"))?;
        set_foreground(&self.tty, group)?;
        self.active = true;
        // The child may have read stdin before the handoff and stopped with SIGTTIN.
        // SAFETY: a negative child ID addresses its own process group, created by Command::process_group.
        unsafe {
            libc::kill(-group, libc::SIGCONT);
        }
        Ok(())
    }
}
impl Drop for Foreground {
    fn drop(&mut self) {
        if self.active {
            let _ = set_foreground(&self.tty, self.previous);
        }
    }
}

struct SignalMask(libc::sigset_t);
impl SignalMask {
    fn block_ttou() -> io::Result<Self> {
        // SAFETY: sigemptyset initializes the sets; pthread_sigmask affects only this thread.
        unsafe {
            let mut set = std::mem::zeroed();
            let mut old = std::mem::zeroed();
            libc::sigemptyset(&raw mut set);
            libc::sigaddset(&raw mut set, libc::SIGTTOU);
            let error = libc::pthread_sigmask(libc::SIG_BLOCK, &raw const set, &raw mut old);
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error));
            }
            Ok(Self(old))
        }
    }
}
impl Drop for SignalMask {
    fn drop(&mut self) {
        // SAFETY: self.0 is the initialized mask saved for this same thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &raw const self.0, std::ptr::null_mut());
        }
    }
}
fn set_foreground(tty: &File, group: libc::pid_t) -> io::Result<()> {
    let _mask = SignalMask::block_ttou()?;
    // SAFETY: the descriptor remains open and the group belongs to the controlling terminal's session.
    if unsafe { libc::tcsetpgrp(tty.as_raw_fd(), group) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nyl_core::orchestration::driver::{ToolExit, ToolInvocation, ToolRunner};
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Runs only inside the session and controlling terminal created below.
    #[test]
    #[ignore = "subprocess helper for the isolated pseudo-terminal test"]
    fn test_interactive_child() {
        // SAFETY: inspect this isolated test process's own session identifiers.
        if unsafe { libc::getpgrp() != libc::getpid() } {
            return;
        }
        let invocation = ToolInvocation {
            program: "sh".into(),
            args: vec!["-c".into(), r#"read line; printf 'child-read:%s\n' "$line""#.into()],
            working_dir: std::env::current_dir().unwrap(),
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into())]),
            stdin: None,
            private_stdout: false,
            forward_stdout: true,
            interactive: true,
            timeout: Some(Duration::from_secs(5)),
            masked: Vec::new(),
        };
        let result = crate::process::ProcessRunner::new().run(invocation).unwrap();
        assert!(matches!(result, ToolExit::Exited { code: 0, .. }), "{result:?}");
        // SAFETY: stdin is this subprocess's controlling terminal.
        assert_eq!(unsafe { libc::tcgetpgrp(0) }, unsafe { libc::getpgrp() });
        println!("PTY-RESTORED");
    }
    #[test]
    fn test_interactive_tool_reads_foreground_terminal_and_restores_parent_group() {
        let mut master = -1;
        let mut slave = -1;
        // SAFETY: openpty initializes two owned descriptors; no termios or window override is needed.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &raw mut master,
                    &raw mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: each newly opened descriptor is adopted exactly once.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "process::terminal::tests::test_interactive_child",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: after fork this closure uses only async-signal-safe libc calls before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(slave);
        master.write_all(b"hello\n").unwrap();
        let start = Instant::now();
        let mut output = Vec::new();
        while start.elapsed() < Duration::from_secs(10) {
            let mut poll = libc::pollfd {
                fd: master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: poll receives one live descriptor and a bounded timeout.
            if unsafe { libc::poll(&raw mut poll, 1, 50) } > 0 {
                let mut buffer = [0; 4096];
                match master.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => output.extend_from_slice(&buffer[..count]),
                }
            }
            if child.try_wait().unwrap().is_some() && String::from_utf8_lossy(&output).contains("PTY-RESTORED") {
                break;
            }
        }
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
        }
        let status = child.wait().unwrap();
        let output = String::from_utf8_lossy(&output);
        assert!(status.success(), "{status}: {output}");
        assert!(output.contains("child-read:hello"), "{output}");
        assert!(output.contains("PTY-RESTORED"), "{output}");
    }
}
