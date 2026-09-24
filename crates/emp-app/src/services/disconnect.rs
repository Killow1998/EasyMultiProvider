//! Request-local downstream disconnect observation for streamed proxy work.

use std::future::Future;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

pub(crate) struct DisconnectMonitor {
    disconnected: tokio::sync::oneshot::Receiver<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub(crate) enum DisconnectRace<T> {
    Ready(T),
    Disconnected,
}

impl DisconnectMonitor {
    pub(crate) fn start(stream: &TcpStream) -> std::io::Result<Self> {
        let probe = stream.try_clone()?;
        probe.set_read_timeout(Some(Duration::from_millis(50)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (sender, disconnected) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            let mut byte = [0_u8; 1];
            while !worker_stop.load(Ordering::Acquire) {
                match probe.peek(&mut byte) {
                    Ok(0) => {
                        let _ = sender.send(());
                        return;
                    }
                    Ok(_) => thread::sleep(Duration::from_millis(10)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => {
                        let _ = sender.send(());
                        return;
                    }
                }
            }
        });
        Ok(Self {
            disconnected,
            stop,
            worker: Some(worker),
        })
    }

    pub(crate) async fn race<F: Future>(&mut self, future: F) -> DisconnectRace<F::Output> {
        tokio::select! {
            biased;
            _ = &mut self.disconnected => DisconnectRace::Disconnected,
            result = future => DisconnectRace::Ready(result),
        }
    }
}

impl Drop for DisconnectMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
