use emp_domain::FixedWait;
use std::{
    borrow::Borrow,
    fmt::{Debug, Display},
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    num::NonZeroU64,
    ops::Add,
    path::PathBuf,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

mod observation;
mod process;
pub mod sse;
#[cfg(feature = "websocket")]
pub mod websocket;
pub use observation::{Observation, ObservationResult, Probe};
pub use process::{ChildError, OwnedChild};

pub fn join_check<T>(worker: thread::JoinHandle<T>, label: &str) -> Option<T> {
    match worker.join() {
        Ok(value) => Some(value),
        Err(payload) if thread::panicking() => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            let _ = writeln!(io::stderr(), "{label}: {message}");
            None
        }
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

pub fn drop_check<T, E: Debug>(result: Result<T, E>, label: &str) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) if thread::panicking() => {
            let _ = writeln!(io::stderr(), "{label}: {error:?}");
            None
        }
        Err(error) => panic!("{label}: {error:?}"),
    }
}

pub fn accept_blocking(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    let (stream, peer) = listener.accept()?;
    stream.set_nonblocking(false)?;
    Ok((stream, peer))
}

pub fn fixture_root() -> io::Result<PathBuf> {
    let root = std::env::var_os("EMP_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    if cfg!(unix) {
        root.canonicalize()
    } else {
        Ok(root)
    }
}

pub fn fixture_dir() -> std::io::Result<tempfile::TempDir> {
    tempfile::tempdir_in(fixture_root()?)
}

/// A test duration between one nanosecond and one day.
#[derive(Clone, Copy, Debug)]
pub struct TestBudget(NonZeroU64);

impl TestBudget {
    const MAX: Duration = Duration::from_secs(24 * 60 * 60);

    fn capped(duration: Duration) -> Self {
        let nanos = duration.min(Self::MAX).as_nanos() as u64;
        Self(NonZeroU64::new(nanos).unwrap_or(NonZeroU64::MIN))
    }

    pub fn with_product_wait(self, wait: FixedWait) -> Self {
        Self::capped(self.duration().saturating_add(wait.get()))
    }

    pub fn duration(self) -> Duration {
        Duration::from_nanos(self.0.get())
    }
}

impl From<FixedWait> for TestBudget {
    fn from(wait: FixedWait) -> Self {
        Self::capped(wait.get())
    }
}

impl Add for TestBudget {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self::capped(self.duration().saturating_add(other.duration()))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Deadline(Instant);

impl Deadline {
    pub fn expired() -> Self {
        Self(Instant::now())
    }

    pub fn after(budget: TestBudget) -> Self {
        Self::starting_at(Instant::now(), budget)
    }

    pub fn starting_at(started_at: Instant, budget: TestBudget) -> Self {
        Self(
            started_at
                .checked_add(budget.duration())
                .unwrap_or(started_at),
        )
    }

    pub fn capped(&self, budget: TestBudget) -> Self {
        Self(self.0.min(Self::after(budget).0))
    }

    pub fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }

    pub fn is_expired(&self) -> bool {
        self.remaining().is_zero()
    }

    pub fn accept<T>(&self, value: T, context: impl Display) -> io::Result<T> {
        self.accept_at(Instant::now(), value, context)
    }

    fn accept_at<T>(
        &self,
        completed_at: Instant,
        value: T,
        context: impl Display,
    ) -> io::Result<T> {
        if completed_at >= self.0 {
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
    pub fn deadline(&self) -> &Deadline {
        &self.deadline
    }

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

/// A positive duration that scales to zero is rounded up to one nanosecond.
pub fn test_timeout(duration: Duration) -> TestBudget {
    const MIN_SCALE: f64 = 1.0 / 16.0;
    const MAX_SCALE: f64 = 64.0;
    static SCALE: OnceLock<f64> = OnceLock::new();
    assert!(
        !duration.is_zero(),
        "test timeout must be positive; use Deadline::expired() for an expired deadline"
    );
    let scale = SCALE.get_or_init(|| match std::env::var("EMP_TEST_TIME_SCALE") {
        Ok(value) => value
            .parse::<f64>()
            .ok()
            .filter(|scale| (MIN_SCALE..=MAX_SCALE).contains(scale))
            .expect("EMP_TEST_TIME_SCALE must be a number between 0.0625 and 64 inclusive"),
        Err(std::env::VarError::NotPresent) => 1.0,
        Err(error) => panic!("EMP_TEST_TIME_SCALE: {error}"),
    });
    let scaled_seconds = (duration.as_secs_f64() * scale).min(TestBudget::MAX.as_secs_f64());
    TestBudget::capped(Duration::from_secs_f64(scaled_seconds))
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
    use crate::process::tests::isolated;
    use std::net::TcpListener;

    #[test]
    #[should_panic(expected = "fixture worker assertion: expected progress")]
    fn join_check_propagates_worker_assertion() {
        let worker = thread::spawn(|| {
            assert_eq!(0, 1, "fixture worker assertion: expected progress");
        });
        join_check(worker, "join fixture");
    }

    #[test]
    fn drop_failure_during_unwind_does_not_abort() {
        const CASE: &str = "EMP_TEST_DROP_FAILURE_DURING_UNWIND";
        if std::env::var_os(CASE).is_some() {
            struct FailingFixture(Option<thread::JoinHandle<()>>);
            impl Drop for FailingFixture {
                fn drop(&mut self) {
                    if let Some(worker) = self.0.take() {
                        join_check(worker, "join failing fixture");
                    }
                }
            }
            let panic = std::panic::catch_unwind(|| {
                let _fixture =
                    FailingFixture(Some(thread::spawn(|| panic!("fixture worker failed"))));
                panic!("original test failed");
            })
            .unwrap_err();
            assert_eq!(
                panic.downcast_ref::<&str>().copied(),
                Some("original test failed")
            );
            isolated::report(Ok(()));
            return;
        }

        let mut command = isolated::command("tests::drop_failure_during_unwind_does_not_abort");
        command.env(CASE, "1");
        let deadline = Deadline::after(test_timeout(Duration::from_secs(5)));
        let output = isolated::run(command, deadline);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("original test failed"), "{stderr}");
        assert!(
            stderr.contains("join failing fixture: fixture worker failed"),
            "{stderr}"
        );
    }
    #[test]
    fn test_budget_construction_and_composition_saturate() {
        let maximum = test_timeout(Duration::MAX);
        assert_eq!(maximum.duration(), TestBudget::MAX);
        assert_eq!((maximum + maximum).duration(), TestBudget::MAX);
        assert_eq!(
            maximum
                .with_product_wait(FixedWait::new(Duration::MAX))
                .duration(),
            TestBudget::MAX
        );
    }

    #[test]
    #[should_panic(expected = "use Deadline::expired() for an expired deadline")]
    fn test_timeout_rejects_zero() {
        test_timeout(Duration::ZERO);
    }

    #[test]
    fn test_timeout_stays_positive_at_minimum_scale() {
        const CASE: &str = "EMP_TEST_MINIMUM_TIME_SCALE";
        if std::env::var_os(CASE).is_some() {
            assert_eq!(
                test_timeout(Duration::from_nanos(1)).duration(),
                Duration::from_nanos(1)
            );
            assert_eq!(
                test_timeout(Duration::from_nanos(32)).duration(),
                Duration::from_nanos(2)
            );
            isolated::report(Ok(()));
            return;
        }

        let mut command = isolated::command("tests::test_timeout_stays_positive_at_minimum_scale");
        command.env(CASE, "1").env("EMP_TEST_TIME_SCALE", "0.0625");
        isolated::run(
            command,
            Deadline::after(test_timeout(Duration::from_secs(5))),
        );
    }

    #[test]
    fn expired_deadline_is_expired_immediately() {
        assert!(Deadline::expired().is_expired());
    }

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
