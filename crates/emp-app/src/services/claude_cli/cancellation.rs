//! Cancellation shared by the temporary TUI, control queries and upstream I/O.
use tokio::sync::watch;

pub(super) struct Cancellation {
    signal: watch::Sender<bool>,
}

impl Cancellation {
    pub(super) fn new() -> Self {
        Self {
            signal: watch::channel(false).0,
        }
    }
    pub(super) fn cancel(&self) {
        self.signal.send_replace(true);
    }
    pub(super) fn is_cancelled(&self) -> bool {
        *self.signal.borrow()
    }
    pub(super) async fn cancelled(&self) {
        let mut receiver = self.signal.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}
