use crate::Deadline;
use std::{
    fmt, io,
    process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

mod capture;
#[cfg(windows)]
mod windows;

use capture::Capture;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub struct ChildError {
    kind: io::ErrorKind,
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ChildError {
    pub fn kind(&self) -> io::ErrorKind {
        self.kind
    }

    pub fn status(&self) -> Option<ExitStatus> {
        self.status
    }

    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }
}

impl From<io::Error> for ChildError {
    fn from(error: io::Error) -> Self {
        Self {
            kind: error.kind(),
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

impl fmt::Display for ChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "child process: {}", self.kind)?;
        if let Some(status) = self.status {
            write!(formatter, " ({status})")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChildError")
            .field("kind", &self.kind)
            .field("status", &self.status)
            .finish()
    }
}

impl std::error::Error for ChildError {}

pub struct OwnedChild {
    child: Child,
    status: Option<ExitStatus>,
    stdout: Option<Arc<Capture>>,
    stderr: Option<Arc<Capture>>,
    #[cfg(windows)]
    job: windows::Job,
}

impl OwnedChild {
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            Self {
                child: command.process_group(0).spawn()?,
                status: None,
                stdout: None,
                stderr: None,
            }
            .start_capture()
        }
        #[cfg(windows)]
        Self::spawn_with_creation_flags(command, 0)
    }

    #[cfg(windows)]
    pub fn spawn_with_creation_flags(command: &mut Command, flags: u32) -> io::Result<Self> {
        let (child, job) = windows::spawn(command, flags)?;
        Self {
            child,
            status: None,
            stdout: None,
            stderr: None,
            job,
        }
        .start_capture()
    }

    fn start_capture(mut self) -> io::Result<Self> {
        self.stdout = self.child.stdout.take().map(Capture::start).transpose()?;
        self.stderr = self.child.stderr.take().map(Capture::start).transpose()?;
        Ok(self)
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn stdout_line(&self, deadline: Deadline) -> io::Result<Option<String>> {
        self.stdout
            .as_ref()
            .map_or(Ok(None), |capture| capture.line(deadline))
    }

    pub fn stderr_line(&self, deadline: Deadline) -> io::Result<Option<String>> {
        self.stderr
            .as_ref()
            .map_or(Ok(None), |capture| capture.line(deadline))
    }

    pub fn output(command: &mut Command, deadline: Deadline) -> Result<Output, ChildError> {
        Self::spawn(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )?
        .wait_until(deadline)
    }

    pub fn wait_until(&mut self, deadline: Deadline) -> Result<Output, ChildError> {
        drop(self.child.stdin.take());
        let completed = self.wait_for_exit_and_output(deadline);
        let terminated = self.terminate();
        let (stdout, stdout_result) = self
            .stdout
            .take()
            .map_or((Vec::new(), Ok(())), |capture| capture.take_output());
        let (stderr, stderr_result) = self
            .stderr
            .take()
            .map_or((Vec::new(), Ok(())), |capture| capture.take_output());
        match completed
            .and(terminated)
            .and_then(|status| stdout_result.and(stderr_result).map(|()| status))
        {
            Ok(status) => Ok(Output {
                status,
                stdout,
                stderr,
            }),
            Err(error) => Err(ChildError {
                kind: error.kind(),
                status: self.status,
                stdout,
                stderr,
            }),
        }
    }

    fn wait_for_exit_and_output(&mut self, deadline: Deadline) -> io::Result<()> {
        loop {
            if (self.status.is_some() || self.has_exited()?)
                && [&self.stdout, &self.stderr]
                    .into_iter()
                    .flatten()
                    .all(|capture| capture.is_finished())
            {
                return deadline.accept((), "child exit and output");
            }
            let remaining = deadline.accept(deadline.remaining(), "child exit and output")?;
            poll_pause(remaining.min(POLL_INTERVAL));
        }
    }

    #[cfg(unix)]
    fn has_exited(&self) -> io::Result<bool> {
        use rustix::{
            io::Errno,
            process::{Pid, WaitId, WaitIdOptions, waitid},
        };
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        match waitid(WaitId::Pid(Pid::from_child(&self.child)), options) {
            Ok(status) => Ok(status.is_some()),
            Err(Errno::INTR) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(windows)]
    fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub fn terminate(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        #[cfg(unix)]
        {
            use rustix::{
                io::Errno,
                process::{Pid, Signal, kill_process_group},
            };
            match kill_process_group(Pid::from_child(&self.child), Signal::KILL) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(windows)]
        self.job.terminate()?;
        let status = self.child.wait()?;
        self.status = Some(status);
        Ok(status)
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.terminate();
        for capture in [&self.stdout, &self.stderr].into_iter().flatten() {
            capture.cancel();
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "按既有固定间隔检查子进程退出和非阻塞输出，保留期限与取消检查"
)]
fn poll_pause(interval: Duration) {
    thread::sleep(interval);
}

#[cfg(test)]
mod tests;
