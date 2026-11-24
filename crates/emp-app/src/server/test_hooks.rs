use super::transport::ConnectionIo;
use crate::services::observation::http::HttpObservation;
use std::task::{Context, Poll};

#[cfg(test)]
pub(super) use implementation::{RequestProbe, WriteProbe};
pub(super) use implementation::{ServerHooks, WriteHook};

#[cfg(not(test))]
mod implementation {
    use super::*;

    #[derive(Default)]
    pub(in crate::server) struct ServerHooks {}

    impl ServerHooks {
        pub(in crate::server) fn request(&self, _: &str, _: &HttpObservation, _: &ConnectionIo) {}

        pub(in crate::server) async fn health_handler(&self, _: bool) {}

        pub(in crate::server) fn output_queued(&self) {}

        pub(in crate::server) async fn before_response_receive(&self) {}
    }

    #[derive(Clone, Default)]
    pub(in crate::server) struct WriteHook {}

    impl WriteHook {
        pub(in crate::server) fn poll(&self, _: &mut Context<'_>, _: &[u8]) -> Poll<()> {
            Poll::Ready(())
        }

        pub(in crate::server) fn cancel(&self) {}
    }
}

#[cfg(test)]
mod implementation {
    use super::*;
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::ready,
    };
    use tokio::sync::oneshot;

    pub(in crate::server) struct RequestProbe {
        pub(in crate::server) path: &'static str,
        pub(in crate::server) id: oneshot::Sender<String>,
        pub(in crate::server) write: Option<WriteProbe>,
    }

    pub(in crate::server) struct WriteProbe {
        pub(in crate::server) marker: &'static [u8],
        pub(in crate::server) reached: Option<oneshot::Sender<()>>,
        pub(in crate::server) release: oneshot::Receiver<()>,
    }

    type HandlerProbe = (oneshot::Sender<()>, oneshot::Receiver<()>);

    #[derive(Default)]
    pub(in crate::server) struct ServerHooks {
        pub(in crate::server) request: Mutex<Option<RequestProbe>>,
        pub(in crate::server) health_handler: Mutex<Option<HandlerProbe>>,
        pub(in crate::server) output_queued: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        pub(in crate::server) response_receive: Mutex<Option<HandlerProbe>>,
    }

    impl ServerHooks {
        pub(in crate::server) fn request(
            &self,
            path: &str,
            observation: &HttpObservation,
            io: &ConnectionIo,
        ) {
            let mut armed = self.request.lock().unwrap();
            if armed.as_ref().is_some_and(|probe| probe.path == path) {
                let probe = armed.take().unwrap();
                if let Some(write) = probe.write {
                    *io.write_hook.0.lock().unwrap() = Some(write);
                }
                let _ = probe.id.send(observation.request_id().unwrap().to_owned());
            }
        }

        pub(in crate::server) async fn health_handler(&self, is_health: bool) {
            if is_health {
                let probe = self.health_handler.lock().unwrap().take();
                if let Some((entered, release)) = probe {
                    let _ = entered.send(());
                    let _ = release.await;
                }
            }
        }

        pub(in crate::server) async fn before_response_receive(&self) {
            let resume = self.response_receive.lock().unwrap().take();
            if let Some((paused, resume)) = resume {
                let _ = paused.send(());
                let _ = resume.await;
            }
        }

        pub(in crate::server) fn output_queued(&self) {
            let probe = self.output_queued.lock().unwrap().take();
            if let Some(probe) = probe {
                probe();
            }
        }
    }

    #[derive(Clone, Default)]
    pub(in crate::server) struct WriteHook(Arc<Mutex<Option<WriteProbe>>>);

    impl WriteHook {
        pub(in crate::server) fn poll(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<()> {
            let mut armed = self.0.lock().unwrap();
            if let Some(probe) = armed.as_mut()
                && (probe.reached.is_none()
                    || buf
                        .windows(probe.marker.len())
                        .any(|bytes| bytes == probe.marker))
            {
                if let Some(reached) = probe.reached.take() {
                    let _ = reached.send(());
                }
                let _ = ready!(Pin::new(&mut probe.release).poll(cx));
                armed.take();
            }
            Poll::Ready(())
        }

        pub(in crate::server) fn cancel(&self) {
            self.0.lock().unwrap().take();
        }
    }
}
