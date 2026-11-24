use super::transport::ConnectionIo;
use crate::services::observation::http::HttpObservation;
use std::{
    sync::{LockResult, Mutex, MutexGuard},
    task::{Context, Poll},
};

pub(crate) use implementation::ServerHooks;
pub(super) use implementation::WriteHook;
#[cfg(test)]
pub(super) use implementation::{RequestProbe, WriteProbe, WriteTrigger};

#[cfg(not(test))]
mod implementation {
    use super::*;

    #[derive(Default)]
    pub(crate) struct ServerHooks {}

    impl ServerHooks {
        pub(in crate::server) fn request(&self, _: &str, _: &HttpObservation, _: &ConnectionIo) {}

        pub(in crate::server) async fn health_handler(&self, _: bool) {}

        pub(in crate::server) fn publish_output<T>(
            &self,
            _: Option<&WriteHook>,
            publish: impl FnOnce() -> T,
        ) -> T {
            publish()
        }

        pub(crate) fn lock_callback_delegations<'a, T>(
            &self,
            delegations: &'a Mutex<T>,
        ) -> LockResult<MutexGuard<'a, T>> {
            delegations.lock()
        }

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
        sync::{Arc, TryLockError},
        task::ready,
    };
    use tokio::sync::oneshot;

    pub(in crate::server) struct RequestProbe {
        pub(in crate::server) path: &'static str,
        pub(in crate::server) id: oneshot::Sender<String>,
        pub(in crate::server) write: Option<WriteProbe>,
    }

    pub(in crate::server) struct WriteProbe {
        pub(in crate::server) trigger: WriteTrigger,
        pub(in crate::server) reached: Option<oneshot::Sender<()>>,
        pub(in crate::server) release: oneshot::Receiver<()>,
    }

    #[derive(Clone, Copy)]
    pub(in crate::server) enum WriteTrigger {
        Bytes(&'static [u8]),
        Output,
    }

    type HandlerProbe = (oneshot::Sender<()>, oneshot::Receiver<()>);
    type OutputProbe = Box<dyn FnOnce() + Send>;

    #[derive(Default)]
    pub(crate) struct ServerHooks {
        pub(in crate::server) request: Mutex<Option<RequestProbe>>,
        pub(in crate::server) health_handler: Mutex<Option<HandlerProbe>>,
        pub(in crate::server) before_output_publish: Mutex<Option<OutputProbe>>,
        pub(in crate::server) output_queued: Mutex<Option<OutputProbe>>,
        pub(in crate::server) response_receive: Mutex<Option<HandlerProbe>>,
        pub(in crate::server) callback_lock_contended: Mutex<Option<oneshot::Sender<bool>>>,
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

        pub(in crate::server) fn publish_output<T>(
            &self,
            write: Option<&WriteHook>,
            publish: impl FnOnce() -> T,
        ) -> T {
            let queued = write.and_then(WriteHook::pause_for_output);
            if queued.is_some() {
                let before_publish = self.before_output_publish.lock().unwrap().take();
                if let Some(before_publish) = before_publish {
                    before_publish();
                }
            }
            let result = publish();
            let probe = self.output_queued.lock().unwrap().take();
            if let Some(probe) = probe {
                probe();
            }
            if let Some(queued) = queued {
                let _ = queued.send(());
            }
            result
        }

        pub(crate) fn lock_callback_delegations<'a, T>(
            &self,
            delegations: &'a Mutex<T>,
        ) -> LockResult<MutexGuard<'a, T>> {
            let probe = self.callback_lock_contended.lock().unwrap().take();
            if let Some(probe) = probe {
                match delegations.try_lock() {
                    Err(TryLockError::WouldBlock) => {
                        let _ = probe.send(true);
                    }
                    result => {
                        let _ = probe.send(false);
                        return result.map_err(|error| match error {
                            TryLockError::Poisoned(error) => error,
                            TryLockError::WouldBlock => unreachable!(),
                        });
                    }
                }
            }
            delegations.lock()
        }
    }

    #[derive(Clone, Default)]
    pub(in crate::server) struct WriteHook(Arc<Mutex<Option<WriteProbe>>>);

    impl WriteHook {
        fn pause_for_output(&self) -> Option<oneshot::Sender<()>> {
            let mut armed = self.0.lock().unwrap();
            let probe = armed.as_mut()?;
            if matches!(probe.trigger, WriteTrigger::Output) {
                return probe.reached.take();
            }
            None
        }

        pub(in crate::server) fn poll(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<()> {
            let mut armed = self.0.lock().unwrap();
            if let Some(probe) = armed.as_mut()
                && (probe.reached.is_none()
                    || match probe.trigger {
                        WriteTrigger::Bytes(marker) => {
                            buf.windows(marker.len()).any(|bytes| bytes == marker)
                        }
                        WriteTrigger::Output => false,
                    })
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
