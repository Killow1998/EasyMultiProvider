//! The temporary quota helper owns its launcher and all inherited children.

use std::io;
use std::process::{Child, Command};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
mod windows;

pub(super) struct QuotaChild {
    pub(super) process: Child,
    stopped: bool,
    #[cfg(windows)]
    job: windows::Job,
}

impl QuotaChild {
    pub(super) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        let (process, job) = windows::spawn(command)?;
        #[cfg(not(windows))]
        let process = command.spawn()?;
        Ok(Self {
            process,
            stopped: false,
            #[cfg(windows)]
            job,
        })
    }

    pub(super) fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // A successful launcher exit does not imply its children exited.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.process.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        self.job.stop();
        let _ = self.process.kill();
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + super::PROCESS_EXIT_TIMEOUT;
        let result = loop {
            match self.process.try_wait() {
                Ok(Some(status)) if status.success() => break Ok(()),
                Ok(Some(_)) => break Err(io::Error::other("quota helper failed")),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    break Err(io::Error::new(io::ErrorKind::TimedOut, "quota helper exit"));
                }
                Err(error) => break Err(error),
            }
        };
        self.stop();
        let _ = self.process.wait();
        result
    }
}

impl Drop for QuotaChild {
    fn drop(&mut self) {
        self.stop();
        let _ = self.process.wait();
    }
}
