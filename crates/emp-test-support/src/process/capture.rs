use super::{POLL_INTERVAL, poll_pause};
use crate::Deadline;
use std::{
    io::{self, Read},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    line_start: usize,
    result: Option<io::Result<()>>,
}

#[derive(Default)]
pub(super) struct Capture {
    output: Mutex<Output>,
    changed: Condvar,
    cancelled: AtomicBool,
}

impl Capture {
    pub(super) fn start(pipe: impl ReadPipe + Send + 'static) -> io::Result<Arc<Self>> {
        let capture = Arc::new(Self::default());
        let worker = Arc::clone(&capture);
        thread::Builder::new().spawn(move || {
            let result = worker.read_output(pipe);
            worker.output.lock().unwrap().result = Some(result);
            worker.changed.notify_all();
        })?;
        Ok(capture)
    }

    fn read_output(&self, mut pipe: impl ReadPipe) -> io::Result<()> {
        pipe.prepare()?;
        let mut buffer = [0; 8192];
        while !self.cancelled.load(Ordering::Relaxed) {
            match pipe.read_available(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let mut output = self.output.lock().unwrap();
                    if self.cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    output.bytes.extend_from_slice(&buffer[..count]);
                    self.changed.notify_all();
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    poll_pause(POLL_INTERVAL)
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    pub(super) fn line(&self, deadline: Deadline) -> io::Result<Option<String>> {
        let mut output = self.output.lock().unwrap();
        let mut inspected = output.line_start;
        loop {
            deadline.accept((), "child output line")?;
            inspected = inspected.max(output.line_start);
            let end =
                if let Some(newline) = output.bytes[inspected..].iter().position(|&b| b == b'\n') {
                    inspected + newline + 1
                } else if let Some(result) = &output.result {
                    if let Err(error) = result {
                        return Err(error.kind().into());
                    }
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

    pub(super) fn is_finished(&self) -> bool {
        self.output.lock().unwrap().result.is_some()
    }

    pub(super) fn take_output(&self) -> (Vec<u8>, io::Result<()>) {
        self.cancel();
        let mut output = self.output.lock().unwrap();
        output.line_start = 0;
        (
            std::mem::take(&mut output.bytes),
            output.result.take().unwrap_or(Ok(())),
        )
    }

    pub(super) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[cfg(unix)]
pub(super) trait ReadPipe: Read + std::os::fd::AsFd {
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
pub(super) trait ReadPipe: Read + std::os::windows::io::AsRawHandle {
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
