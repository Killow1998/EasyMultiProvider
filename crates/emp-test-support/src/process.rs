use crate::{Deadline, TestBudget, test_timeout};
use std::{
    fmt, io,
    io::Write as _,
    process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio},
    thread,
    time::Duration,
};

mod capture;
#[cfg(windows)]
mod windows;

use capture::Capture;

const POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Io(io::ErrorKind),
    CaptureTimeout,
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(kind) => kind.fmt(formatter),
            Self::CaptureTimeout => {
                formatter.write_str("output reader did not exit before cleanup deadline")
            }
        }
    }
}

pub struct ChildError {
    failure: Failure,
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ChildError {
    pub fn kind(&self) -> io::ErrorKind {
        match self.failure {
            Failure::Io(kind) => kind,
            Failure::CaptureTimeout => io::ErrorKind::TimedOut,
        }
    }

    pub fn capture_timed_out(&self) -> bool {
        self.failure == Failure::CaptureTimeout
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
            failure: Failure::Io(error.kind()),
            status: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }
}

impl fmt::Display for ChildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "child process: {}", self.failure)?;
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
            .field("failure", &self.failure)
            .field("status", &self.status)
            .finish()
    }
}

impl std::error::Error for ChildError {}

pub struct OwnedChild {
    child: Child,
    status: Option<ExitStatus>,
    stdout: Option<Capture>,
    stderr: Option<Capture>,
    reader_cleanup: TestBudget,
    #[cfg(windows)]
    job: windows::Job,
}

impl OwnedChild {
    pub fn spawn(command: Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let mut command = command;
            let reader_cleanup = test_timeout(Duration::from_secs(5));
            Self {
                child: command.process_group(0).spawn()?,
                status: None,
                stdout: None,
                stderr: None,
                reader_cleanup,
            }
            .start_capture()
        }
        #[cfg(windows)]
        Self::spawn_with_creation_flags(command, 0)
    }

    #[cfg(windows)]
    pub fn spawn_with_creation_flags(command: Command, flags: u32) -> io::Result<Self> {
        let reader_cleanup = test_timeout(Duration::from_secs(5));
        let (child, job) = windows::spawn(command, flags)?;
        Self {
            child,
            status: None,
            stdout: None,
            stderr: None,
            reader_cleanup,
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

    pub fn output(mut command: Command, deadline: Deadline) -> Result<Output, ChildError> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self::spawn(command)?.wait_until(deadline)
    }

    pub fn wait_until(&mut self, deadline: Deadline) -> Result<Output, ChildError> {
        drop(self.child.stdin.take());
        let completed = self.wait_for_exit_and_output(deadline);
        let terminated = self.terminate();
        let cleanup = Deadline::after(self.reader_cleanup);
        let (stdout, stdout_result) = finish_capture(self.stdout.take(), cleanup);
        let (stderr, stderr_result) = finish_capture(self.stderr.take(), cleanup);
        let result = if [stdout_result, stderr_result].contains(&Err(Failure::CaptureTimeout)) {
            Err(Failure::CaptureTimeout)
        } else {
            completed
                .and(terminated)
                .map_err(|error| Failure::Io(error.kind()))
                .and_then(|status| stdout_result.and(stderr_result).map(|()| status))
        };
        match result {
            Ok(status) => Ok(Output {
                status,
                stdout,
                stderr,
            }),
            Err(failure) => Err(ChildError {
                failure,
                status: self.status,
                stdout,
                stderr,
            }),
        }
    }

    fn wait_for_exit_and_output(&mut self, deadline: Deadline) -> io::Result<()> {
        while self.status.is_none() && !self.has_exited()? {
            let remaining = deadline.accept(deadline.remaining(), "child exit and output")?;
            poll_pause(remaining.min(POLL_INTERVAL));
        }
        for capture in [&self.stdout, &self.stderr].into_iter().flatten() {
            if !capture.wait_closed(deadline) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "child output"));
            }
        }
        deadline.accept((), "child exit and output")
    }

    #[cfg(unix)]
    fn has_exited(&self) -> io::Result<bool> {
        use rustix::{
            io::Errno,
            process::{Pid, WaitId, WaitIdOptions, waitid},
        };
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        #[expect(
            clippy::disallowed_methods,
            reason = "只观察测试夹具持有的子进程，保留其回收所有权"
        )]
        let observed = waitid(WaitId::Pid(Pid::from_child(&self.child)), options);
        match observed {
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
        self.terminate_group()?;
        #[cfg(windows)]
        self.job.terminate()?;
        let status = self.child.wait()?;
        self.status = Some(status);
        Ok(status)
    }

    /// XNU [killpg1](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_sig.c#L1709-L1718)
    /// skips zombies. All members are test children with the same credentials,
    /// so EPERM after the leader exits means only zombies remain.
    #[cfg(unix)]
    fn terminate_group(&self) -> io::Result<()> {
        use rustix::{
            io::Errno,
            process::{Pid, Signal, kill_process_group},
        };
        match kill_process_group(Pid::from_child(&self.child), Signal::KILL) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(Errno::PERM)
                if cfg!(target_os = "macos") && matches!(self.has_exited(), Ok(true)) =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Err(error) = self.terminate() {
            let _ = writeln!(io::stderr(), "child process cleanup: {}", error.kind());
        }
        let deadline = Deadline::after(self.reader_cleanup);
        for capture in [&mut self.stdout, &mut self.stderr] {
            if let Err(error) = finish_capture(capture.take(), deadline).1 {
                let _ = writeln!(io::stderr(), "child output cleanup: {error}");
            }
        }
    }
}

fn finish_capture(capture: Option<Capture>, deadline: Deadline) -> (Vec<u8>, Result<(), Failure>) {
    capture.map_or_else(|| (Vec::new(), Ok(())), |capture| capture.finish(deadline))
}

#[expect(
    clippy::disallowed_methods,
    reason = "按既有固定间隔检查子进程退出，保留期限检查"
)]
fn poll_pause(interval: Duration) {
    thread::sleep(interval);
}

#[cfg(test)]
pub(crate) mod tests;
