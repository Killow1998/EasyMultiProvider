use super::{Failure, POLL_INTERVAL};
use crate::Deadline;
use std::{
    io::{self, Read},
    sync::{Arc, Condvar, Mutex, PoisonError},
    thread::{self, JoinHandle},
    time::Instant,
};

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    line_start: usize,
    completion: Option<(Instant, Result<(), io::ErrorKind>)>,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    output: Mutex<Output>,
    changed: Condvar,
}

pub(super) struct Capture {
    shared: Arc<Shared>,
    worker: JoinHandle<()>,
}

impl Capture {
    pub(super) fn start(pipe: impl NonblockingPipe + Send + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let reader = Arc::clone(&shared);
        let worker = thread::Builder::new().spawn(move || {
            let result = reader.read_output(pipe);
            reader.output.lock().unwrap().completion =
                Some((Instant::now(), result.map_err(|error| error.kind())));
            reader.changed.notify_all();
        })?;
        Ok(Self { shared, worker })
    }

    pub(super) fn line(&self, deadline: Deadline) -> io::Result<Option<String>> {
        self.shared.line(deadline)
    }

    pub(super) fn wait_closed(&self, deadline: Deadline) -> bool {
        let output = self
            .shared
            .output
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (output, _) = self
            .shared
            .changed
            .wait_timeout_while(output, deadline.remaining(), |output| {
                output.completion.is_none()
            })
            .unwrap_or_else(PoisonError::into_inner);
        output.completion.is_some()
    }

    pub(super) fn finish(self, deadline: Deadline) -> (Vec<u8>, Result<(), Failure>) {
        let closed = self.wait_closed(deadline);
        if !closed {
            self.shared
                .output
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stop = true;
            self.shared.changed.notify_all();
        }
        let joined = self.worker.join();
        let mut output = self
            .shared
            .output
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let bytes = std::mem::take(&mut output.bytes);
        let result = match (joined, closed, output.completion) {
            (Err(_), _, _) => Err(Failure::Io(io::ErrorKind::Other)),
            (Ok(()), true, Some((closed_at, result))) => deadline
                .accept_at(closed_at, result, "child output cleanup")
                .map_err(|_| Failure::CaptureTimeout)
                .and_then(|result| result.map_err(Failure::Io)),
            _ => Err(Failure::CaptureTimeout),
        };
        (bytes, result)
    }
}

impl Shared {
    /// The reader must never block in a pipe syscall so stop can always wake and join it.
    fn read_output(&self, mut pipe: impl NonblockingPipe) -> io::Result<()> {
        pipe.prepare()?;
        let mut buffer = [0; 8192];
        loop {
            let available = pipe.read_limit(buffer.len());
            let mut output = self.output.lock().unwrap();
            if output.stop {
                break;
            }
            let read = match available {
                Ok(0) => break,
                Ok(limit) => pipe.read(&mut buffer[..limit]),
                Err(error) => Err(error),
            };
            match read {
                Ok(0) => break,
                Ok(count) => {
                    output.bytes.extend_from_slice(&buffer[..count]);
                    self.changed.notify_all();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if self
                        .changed
                        .wait_timeout(output, POLL_INTERVAL)
                        .unwrap()
                        .0
                        .stop
                    {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn line(&self, deadline: Deadline) -> io::Result<Option<String>> {
        let mut output = self.output.lock().unwrap();
        let mut inspected = output.line_start;
        loop {
            deadline.accept((), "child output line")?;
            inspected = inspected.max(output.line_start);
            let end =
                if let Some(newline) = output.bytes[inspected..].iter().position(|&b| b == b'\n') {
                    inspected + newline + 1
                } else if let Some((_, result)) = output.completion {
                    result?;
                    if output.line_start == output.bytes.len() {
                        return Ok(None);
                    }
                    output.bytes.len()
                } else {
                    inspected = output.bytes.len();
                    output = self
                        .changed
                        .wait_timeout(output, deadline.remaining())
                        .unwrap()
                        .0;
                    continue;
                };
            let mut line = &output.bytes[output.line_start..end];
            if let Some(without_newline) = line.strip_suffix(b"\n") {
                line = without_newline
                    .strip_suffix(b"\r")
                    .unwrap_or(without_newline);
            }
            let line = std::str::from_utf8(line)
                .map_err(|_| io::ErrorKind::InvalidData)?
                .to_owned();
            output.line_start = end;
            return deadline.accept(Some(line), "child output line");
        }
    }
}

#[cfg(unix)]
pub(super) trait NonblockingPipe: Read + std::os::fd::AsFd {
    fn prepare(&self) -> io::Result<()> {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        fcntl_setfl(self, fcntl_getfl(self)? | OFlags::NONBLOCK)?;
        Ok(())
    }

    fn read_limit(&self, capacity: usize) -> io::Result<usize> {
        Ok(capacity)
    }
}

#[cfg(windows)]
pub(super) trait NonblockingPipe: Read + std::os::windows::io::AsRawHandle {
    fn prepare(&self) -> io::Result<()> {
        Ok(())
    }

    fn read_limit(&self, capacity: usize) -> io::Result<usize> {
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
        Ok(capacity.min(available as usize))
    }
}

impl NonblockingPipe for std::process::ChildStdout {}
impl NonblockingPipe for std::process::ChildStderr {}

#[cfg(test)]
pub(super) mod tests;
