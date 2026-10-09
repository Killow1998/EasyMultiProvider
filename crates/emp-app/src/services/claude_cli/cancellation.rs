//! One Claude invocation's cancellation signal for sockets and upstream I/O.
use polling::{Event, Events, Poller};
use std::io;
use std::time::Instant;
use tokio::sync::watch;

pub(super) struct Cancellation {
    signal: watch::Sender<bool>,
    pub(super) poller: Poller,
}

impl Cancellation {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self {
            signal: watch::channel(false).0,
            poller: Poller::new()?,
        })
    }

    pub(super) fn cancel(&self) {
        if !self.signal.send_replace(true) {
            let _ = self.poller.notify();
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        *self.signal.borrow()
    }

    pub(super) async fn cancelled(&self) {
        let mut receiver = self.signal.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }

    /// The relay owns at most one registered listener/socket at a time.
    pub(super) fn wait(&self, interest: Event, deadline: Option<Instant>) -> io::Result<()> {
        let mut events = Events::with_capacity(std::num::NonZeroUsize::new(1).unwrap());
        loop {
            if self.is_cancelled() {
                return Err(io::ErrorKind::ConnectionAborted.into());
            }
            let timeout =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if timeout.is_some_and(|timeout| timeout.is_zero()) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            events.clear();
            match self.poller.wait(&mut events, timeout) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => {
                    result?;
                }
            }
            if events.iter().any(|event| {
                event.key == interest.key
                    && ((interest.readable && event.readable)
                        || (interest.writable && event.writable))
            }) {
                if self.is_cancelled() {
                    return Err(io::ErrorKind::ConnectionAborted.into());
                }
                return Ok(());
            }
        }
    }
}
