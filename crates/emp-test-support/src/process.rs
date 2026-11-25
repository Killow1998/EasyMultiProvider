use crate::Deadline;
use std::{
    io::{self, Read},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const POLL_INTERVAL: Duration = Duration::from_millis(10);

pub struct OwnedChild(Child);

impl OwnedChild {
    pub fn new(child: Child) -> Self {
        Self(child)
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }

    pub fn output(command: &mut Command, deadline: Deadline) -> io::Result<Output> {
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Self::new(child).wait_until(deadline)
    }

    pub fn wait_until(&mut self, deadline: Deadline) -> io::Result<Output> {
        drop(self.0.stdin.take());
        let stdout = self.0.stdout.take().map(Capture::start);
        let stderr = self.0.stderr.take().map(Capture::start);
        let status = self.wait_for_exit(deadline, || {
            [&stdout, &stderr]
                .into_iter()
                .flatten()
                .all(Capture::is_finished)
        });
        if status.is_err() {
            let _ = self.terminate();
        }
        let (stdout, stdout_result) = Capture::finish(stdout);
        let (stderr, stderr_result) = Capture::finish(stderr);
        let status = status.and_then(|status| stdout_result.and(stderr_result).map(|()| status));
        match status {
            Ok(status) => Ok(Output {
                status,
                stdout,
                stderr,
            }),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}\nstdout:\n{}\nstderr:\n{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr)
                ),
            )),
        }
    }

    fn wait_for_exit(
        &mut self,
        deadline: Deadline,
        output_finished: impl Fn() -> bool,
    ) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.0.try_wait()?
                && output_finished()
            {
                return deadline.accept(status, "child exit and output");
            }
            let remaining = deadline.accept(deadline.remaining(), "child exit and output")?;
            poll_pause(remaining.min(POLL_INTERVAL));
        }
    }

    pub fn terminate(&mut self) -> io::Result<()> {
        if let Ok(Some(_)) = self.0.try_wait() {
            return Ok(());
        }
        self.0.kill()?;
        self.0.wait()?;
        Ok(())
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

struct Capture {
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<(Vec<u8>, io::Result<()>)>>,
}

impl Capture {
    fn start(pipe: impl ReadPipe + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::clone(&stop);
        let reader = thread::spawn(move || {
            let mut output = Vec::new();
            let result = read_output(pipe, &cancelled, &mut output);
            (output, result)
        });
        Self {
            stop,
            reader: Some(reader),
        }
    }

    fn is_finished(&self) -> bool {
        self.reader.as_ref().unwrap().is_finished()
    }

    fn finish(capture: Option<Self>) -> (Vec<u8>, io::Result<()>) {
        capture.map_or((Vec::new(), Ok(())), |mut capture| {
            capture.join().unwrap().unwrap_or_else(|_| {
                (
                    Vec::new(),
                    Err(io::Error::other("child output reader panicked")),
                )
            })
        })
    }

    fn join(&mut self) -> Option<thread::Result<(Vec<u8>, io::Result<()>)>> {
        let reader = self.reader.take()?;
        self.stop.store(true, Ordering::Relaxed);
        Some(reader.join())
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

fn read_output(mut pipe: impl ReadPipe, stop: &AtomicBool, output: &mut Vec<u8>) -> io::Result<()> {
    pipe.prepare()?;
    let mut buffer = [0; 8192];
    while !stop.load(Ordering::Relaxed) {
        match pipe.read_available(&mut buffer) {
            Ok(0) => break,
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => poll_pause(POLL_INTERVAL),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[expect(
    clippy::disallowed_methods,
    reason = "以固定间隔检查子进程退出和输出管道，在期限内保留取消机会"
)]
fn poll_pause(interval: Duration) {
    thread::sleep(interval);
}

#[cfg(unix)]
trait ReadPipe: Read + std::os::fd::AsFd {
    fn prepare(&self) -> io::Result<()> {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        fcntl_setfl(self, fcntl_getfl(self)? | OFlags::NONBLOCK)?;
        Ok(())
    }

    fn read_available(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read(buffer)
    }
}

#[cfg(windows)]
trait ReadPipe: Read + std::os::windows::io::AsRawHandle {
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }

    fn read_available(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        use std::ptr::null_mut;
        use windows_sys::Win32::{Foundation::ERROR_BROKEN_PIPE, System::Pipes::PeekNamedPipe};

        let mut available = 0;
        // SAFETY: This reader owns the pipe handle and `available` is a writable DWORD.
        let ok = unsafe {
            PeekNamedPipe(
                self.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                &mut available,
                null_mut(),
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                Ok(0)
            } else {
                Err(error)
            };
        }
        if available == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let count = buffer.len().min(available as usize);
        self.read(&mut buffer[..count])
    }
}

impl ReadPipe for std::process::ChildStdout {}
impl ReadPipe for std::process::ChildStderr {}

#[cfg(all(test, unix))]
mod tests;
