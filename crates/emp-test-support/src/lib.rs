use std::{
    borrow::Borrow,
    fmt::Display,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
    sync::OnceLock,
    time::{Duration, Instant},
};

pub fn fixture_root() -> PathBuf {
    std::env::var_os("EMP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

pub fn fixture_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(fixture_root())
}

#[derive(Clone, Copy, Debug)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn after(timeout: Duration) -> Self {
        Self(Instant::now() + timeout)
    }

    pub fn capped(&self, timeout: Duration) -> Self {
        Self(self.0.min(Self::after(timeout).0))
    }

    pub fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    pub fn is_expired(&self) -> bool {
        self.remaining().is_zero()
    }

    pub fn accept<T>(&self, value: T, context: impl Display) -> io::Result<T> {
        if self.is_expired() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, context.to_string()));
        }
        Ok(value)
    }

    pub fn connect(&self, address: SocketAddr) -> io::Result<TcpStream> {
        let remaining = self.accept(self.remaining(), "TCP connect deadline")?;
        self.accept(
            TcpStream::connect_timeout(&address, remaining),
            "TCP connect deadline",
        )?
    }

    pub fn io<T: Borrow<TcpStream>>(&self, stream: T) -> DeadlineIo<T> {
        DeadlineIo {
            stream,
            deadline: *self,
        }
    }
}

/// Applies a deadline to reads, peeks, and writes on a blocking TCP stream.
/// The deadline is checked before starting the operation. Once started, a
/// successful operation returns its byte count even if the deadline passes.
/// Socket timeouts are reported as `TimedOut`. Changing the deadline leaves
/// the stream and its position intact.
pub struct DeadlineIo<T> {
    stream: T,
    deadline: Deadline,
}

impl<T> DeadlineIo<T> {
    pub fn set_deadline(&mut self, deadline: &Deadline) {
        self.deadline = *deadline;
    }
}

impl<T: Borrow<TcpStream>> Read for DeadlineIo<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_with(|mut stream| stream.read(buf))
    }
}

impl<T: Borrow<TcpStream>> DeadlineIo<T> {
    pub fn peek(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.read_with(|stream| stream.peek(buf))
    }

    fn read_with(&self, read: impl FnOnce(&TcpStream) -> io::Result<usize>) -> io::Result<usize> {
        let stream = self.stream.borrow();
        let remaining = self
            .deadline
            .accept(self.deadline.remaining(), "TCP read deadline")?;
        stream.set_read_timeout(Some(remaining))?;
        read(stream).map_err(socket_timeout)
    }
}

impl<T: Borrow<TcpStream>> Write for DeadlineIo<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut stream = self.stream.borrow();
        let remaining = self
            .deadline
            .accept(self.deadline.remaining(), "TCP write deadline")?;
        stream.set_write_timeout(Some(remaining))?;
        stream.write(buf).map_err(socket_timeout)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.borrow().flush()
    }
}

fn socket_timeout(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::WouldBlock {
        io::Error::new(io::ErrorKind::TimedOut, error)
    } else {
        error
    }
}

pub fn test_timeout(duration: Duration) -> Duration {
    static SCALE: OnceLock<f64> = OnceLock::new();
    let scale = SCALE.get_or_init(|| match std::env::var("EMP_TEST_TIME_SCALE") {
        Ok(value) => value
            .parse::<f64>()
            .ok()
            .filter(|scale| scale.is_finite() && *scale > 0.0)
            .expect("EMP_TEST_TIME_SCALE must be a positive finite number"),
        Err(std::env::VarError::NotPresent) => 1.0,
        Err(error) => panic!("EMP_TEST_TIME_SCALE: {error}"),
    });
    duration.mul_f64(*scale)
}

#[cfg(unix)]
pub fn write_executable(path: &std::path::Path, script: &str) {
    let status = std::process::Command::new("/bin/sh")
        .args([
            "-ec",
            "umask 077; printf '%s' \"$2\" > \"$1\"; chmod 700 \"$1\"",
            "fixture",
        ])
        .arg(path)
        .arg(script)
        .status()
        .expect("write fixture executable");
    assert!(
        status.success(),
        "write fixture executable at {path:?}: {status}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn read_returns_partial_frame_even_if_the_deadline_expires_after_consuming_it() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (stream, _) = listener.accept().unwrap();
        let first = [0x81, 5, b'h', b'e'];
        peer.write_all(&first).unwrap();
        let deadline = Deadline::after(test_timeout(Duration::from_millis(200)));
        let mut input = deadline.io(stream);
        let mut bytes = [0; 4];
        let count = input
            .read_with(|mut stream| {
                stream.read_exact(&mut bytes)?;
                #[expect(
                    clippy::disallowed_methods,
                    reason = "模型化读取取走字节后恰好跨过期限"
                )]
                std::thread::sleep(deadline.remaining());
                Ok(bytes.len())
            })
            .unwrap();
        assert_eq!(&bytes[..count], &first);

        peer.write_all(b"llo").unwrap();
        assert_eq!(
            input.read(&mut bytes).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        input.set_deadline(&Deadline::after(test_timeout(Duration::from_secs(5))));
        input.read_exact(&mut bytes[..3]).unwrap();
        assert_eq!(&bytes[..3], b"llo");
    }
}
