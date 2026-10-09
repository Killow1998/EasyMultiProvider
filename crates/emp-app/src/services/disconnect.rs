//! Request-local downstream disconnect observation for streamed proxy work.

use polling::{Event, Events, Poller};
use std::future::Future;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

/// A registered clone never reads downstream bytes. It is deregistered and its
/// shared socket timeout restored before the clone is closed.
struct Probe {
    stream: TcpStream,
    poller: Arc<Poller>,
    previous_timeout: Option<Duration>,
}

impl Probe {
    fn new(stream: &TcpStream, poller: Arc<Poller>) -> std::io::Result<Self> {
        let stream = stream.try_clone()?;
        let previous_timeout = stream.read_timeout()?;
        // SAFETY: Probe owns the stream and always deletes its registration in
        // Drop, including early errors and worker unwinding.
        unsafe {
            poller.add(&stream, Event::readable(0))?;
        }
        let probe = Self {
            stream,
            poller,
            previous_timeout,
        };
        // Readiness can be spurious. Bound only that exceptional peek; ordinary
        // idle waits are interruptible and do not wake on a socket timer.
        probe
            .stream
            .set_read_timeout(Some(Duration::from_millis(50)))?;
        Ok(probe)
    }

    fn wait_for_disconnect(&self, stop: &AtomicBool) -> std::io::Result<bool> {
        let mut events = Events::with_capacity(std::num::NonZeroUsize::new(1).unwrap());
        let mut byte = [0_u8; 1];
        while !stop.load(Ordering::Acquire) {
            events.clear();
            match self.poller.wait(&mut events, None) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => {
                    result?;
                }
            }
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            if !events.iter().any(|event| event.readable) {
                continue;
            }
            match self.stream.peek(&mut byte) {
                Ok(0) => return Ok(true),
                // A pipelined request or WS frame belongs to the connection
                // reader. Leave it intact and avoid a readability busy loop.
                Ok(_) => thread::park_timeout(Duration::from_millis(10)),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
            if !stop.load(Ordering::Acquire) {
                self.poller.modify(&self.stream, Event::readable(0))?;
            }
        }
        Ok(false)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        let _ = self.poller.delete(&self.stream);
        let _ = self.stream.set_read_timeout(self.previous_timeout);
    }
}

pub(crate) struct DisconnectMonitor {
    disconnected: tokio::sync::oneshot::Receiver<()>,
    pending: Option<(Probe, tokio::sync::oneshot::Sender<()>)>,
    wake: Arc<Poller>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    interrupted: Option<Arc<AtomicBool>>,
}

pub(crate) enum DisconnectRace<T> {
    Ready(T),
    Disconnected,
}

impl DisconnectMonitor {
    /// A WebSocket turn already owns its framed reader. Reuse the cancellation
    /// race without peeking or starting a competing socket reader.
    pub(crate) fn from_signal(
        disconnected: tokio::sync::oneshot::Receiver<()>,
        interrupted: Arc<AtomicBool>,
    ) -> std::io::Result<Self> {
        Ok(Self {
            disconnected,
            pending: None,
            wake: Arc::new(Poller::new()?),
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            interrupted: Some(interrupted),
        })
    }
    pub(crate) fn is_interrupted(&self) -> bool {
        self.interrupted
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
    }
    pub(crate) fn start(stream: &TcpStream) -> std::io::Result<Self> {
        let wake = Arc::new(Poller::new()?);
        let probe = Probe::new(stream, Arc::clone(&wake))?;
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, disconnected) = tokio::sync::oneshot::channel();
        Ok(Self {
            disconnected,
            pending: Some((probe, sender)),
            wake,
            stop,
            worker: None,
            interrupted: None,
        })
    }

    // Context assessment is normally synchronous and never calls race(). Do
    // not spawn (then join) a socket reader unless a caller actually awaits I/O.
    fn activate(&mut self) {
        let Some((probe, sender)) = self.pending.take() else {
            return;
        };
        let worker_stop = Arc::clone(&self.stop);
        self.worker = Some(thread::spawn(move || {
            if !matches!(probe.wait_for_disconnect(&worker_stop), Ok(false)) {
                let _ = sender.send(());
            }
        }));
    }

    pub(crate) async fn race<F: Future>(&mut self, future: F) -> DisconnectRace<F::Output> {
        self.activate();
        tokio::select! {
            biased;
            _ = &mut self.disconnected => DisconnectRace::Disconnected,
            result = future => DisconnectRace::Ready(result),
        }
    }
}

/// Await `future`, abandoning it as soon as the monitored peer disconnects.
///
/// The single call site pattern that every streamed proxy loop previously
/// hand-rolled (`match monitor { Some(..) => race, None => Ready(block_on) }`).
pub(crate) fn raced<T, F>(
    runtime: &tokio::runtime::Runtime,
    monitor: Option<&mut DisconnectMonitor>,
    future: F,
) -> DisconnectRace<T>
where
    F: Future<Output = T>,
{
    match monitor {
        Some(monitor) => runtime.block_on(monitor.race(future)),
        None => DisconnectRace::Ready(runtime.block_on(future)),
    }
}

impl Drop for DisconnectMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            // Cover both the readiness wait and the unread-data backoff.
            let _ = self.wake.notify();
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn connection() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        (listener.accept().unwrap().0, peer)
    }

    #[test]
    fn unused_monitor_leaves_the_connection_usable() {
        let (mut stream, mut peer) = connection();
        let timeout = Some(Duration::from_secs(3));
        stream.set_read_timeout(timeout).unwrap();
        drop(DisconnectMonitor::start(&stream).unwrap());
        assert_eq!(stream.read_timeout().unwrap(), timeout);
        peer.write_all(b"request").unwrap();
        let mut bytes = [0; 7];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"request");
    }

    #[test]
    fn dropping_an_active_monitor_preserves_unread_data_and_socket_settings() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for queued in [false, true] {
            let (mut stream, mut peer) = connection();
            let timeout = Some(Duration::from_secs(3));
            stream.set_read_timeout(timeout).unwrap();
            let mut monitor = DisconnectMonitor::start(&stream).unwrap();
            if queued {
                peer.write_all(b"next").unwrap();
            }
            assert!(matches!(
                runtime.block_on(monitor.race(async {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                })),
                DisconnectRace::Ready(())
            ));
            drop(monitor);
            assert_eq!(stream.read_timeout().unwrap(), timeout);
            if !queued {
                peer.write_all(b"next").unwrap();
            }
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).unwrap();
            assert_eq!(&bytes, b"next");
        }
    }

    #[test]
    fn monitor_can_complete_work_then_detect_a_disconnect() {
        let (stream, peer) = connection();
        let mut monitor = DisconnectMonitor::start(&stream).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            assert!(matches!(
                monitor.race(async { 7 }).await,
                DisconnectRace::Ready(7)
            ));
            drop(peer);
            let outcome = tokio::time::timeout(
                Duration::from_secs(2),
                monitor.race(std::future::pending::<()>()),
            )
            .await
            .unwrap();
            assert!(matches!(outcome, DisconnectRace::Disconnected));
        });
    }
}
