//! Register all supported termination notifications before applying integration.
use crate::app::ServerState;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// The listener stays active during startup and worker drains as well as the
/// main wait loop. Restoration still runs inside the exiting EMP process.
pub(super) struct ExitSignals {
    task: tokio::task::JoinHandle<()>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    external: Arc<AtomicBool>,
    #[cfg(windows)]
    _session: super::windows::SessionEvents,
    // Keep the runtime alive until Drop joins any in-flight exit callback.
    state: Arc<ServerState>,
}

impl ExitSignals {
    pub(super) fn new(state: Arc<ServerState>) -> io::Result<Self> {
        let mut incoming = IncomingSignals::new()?;
        let external = Arc::new(AtomicBool::new(false));
        #[cfg(windows)]
        let session =
            super::windows::SessionEvents::new(Arc::clone(&state), Arc::clone(&external))?;
        let event = Arc::clone(&external);
        let weak = Arc::downgrade(&state);
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = incoming.recv() => {},
                _ = &mut stopped => return,
            }
            event.store(true, Ordering::Release);
            if let Some(state) = weak.upgrade() {
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = super::shutdown::prepare_exit(&state, false);
                })
                .await;
            }
            // Keep the receivers registered until shutdown completes. A second
            // close notification must not fall through to the default handler.
            let _ = stopped.await;
        });
        Ok(Self {
            task,
            stop: Some(stop),
            external,
            #[cfg(windows)]
            _session: session,
            state,
        })
    }

    pub(super) fn external_exit(&self) -> bool {
        self.external.load(Ordering::Acquire)
    }
}

impl Drop for ExitSignals {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        // run_server owns this guard outside block_on. Do not detach a local
        // restore or drop its runtime on a blocking worker during process exit.
        let _ = self
            .state
            .backend
            .transport
            .runtime
            .block_on(&mut self.task);
    }
}

#[cfg(unix)]
struct IncomingSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl IncomingSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
            _ = self.hangup.recv() => {},
        }
    }
}

#[cfg(windows)]
struct IncomingSignals {
    interrupt: tokio::signal::windows::CtrlC,
    terminate: tokio::signal::windows::CtrlBreak,
    close: tokio::signal::windows::CtrlClose,
    logoff: tokio::signal::windows::CtrlLogoff,
    shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl IncomingSignals {
    fn new() -> io::Result<Self> {
        use tokio::signal::windows;
        Ok(Self {
            interrupt: windows::ctrl_c()?,
            terminate: windows::ctrl_break()?,
            close: windows::ctrl_close()?,
            logoff: windows::ctrl_logoff()?,
            shutdown: windows::ctrl_shutdown()?,
        })
    }

    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {},
            _ = self.terminate.recv() => {},
            _ = self.close.recv() => {},
            _ = self.logoff.recv() => {},
            _ = self.shutdown.recv() => {},
        }
    }
}
