//! Bounded pipe-reader shutdown, including children that detach from a launcher.

use std::io::{self, Read};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

#[cfg(unix)]
pub(super) trait Pipe: Read + AsRawFd {}
#[cfg(unix)]
impl<T: Read + AsRawFd> Pipe for T {}
#[cfg(windows)]
pub(super) trait Pipe: Read + AsRawHandle {}
#[cfg(windows)]
impl<T: Read + AsRawHandle> Pipe for T {}

pub(super) struct CancellablePipe<P> {
    pipe: P,
    stop: Arc<AtomicBool>,
}

impl<P: Pipe> CancellablePipe<P> {
    pub(super) fn new(pipe: P, stop: Arc<AtomicBool>) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let fd = pipe.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { pipe, stop })
    }
}

impl<P: Pipe> Read for CancellablePipe<P> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        while !buffer.is_empty() && !self.stop.load(Ordering::Acquire) {
            #[cfg(windows)]
            let buffer = {
                use windows_sys::Win32::{
                    Foundation::ERROR_BROKEN_PIPE, System::Pipes::PeekNamedPipe,
                };
                let mut available = 0;
                if unsafe {
                    PeekNamedPipe(
                        self.pipe.as_raw_handle(),
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null_mut(),
                        &mut available,
                        std::ptr::null_mut(),
                    )
                } == 0
                {
                    let error = io::Error::last_os_error();
                    return if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) {
                        Ok(0)
                    } else {
                        Err(error)
                    };
                }
                if available == 0 {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let count = buffer.len().min(available as usize);
                &mut buffer[..count]
            };
            match self.pipe.read(buffer) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(20))
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => return result,
            }
        }
        Ok(0)
    }
}
