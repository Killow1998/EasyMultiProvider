use super::{ConnectionAdmission, ServerMode, transport::ConnectionIo};
use crate::services::observation::http::HttpObservation;
use crate::{modules::registry::ActiveModule, services::gateway_clients::Caller};
use axum::body::Bytes;
use emp_storage::diagnostics::Journal;
use std::{
    io,
    sync::{Arc, LockResult, Mutex, MutexGuard, atomic::AtomicBool},
    task::{Context, Poll},
};

#[cfg(test)]
pub(crate) use implementation::RECOVERY_ACCEPT_FAILURE;
pub(super) use implementation::WriteHook;
#[cfg(test)]
pub(super) use implementation::{ReportGate, RequestProbe, WriteProbe, WriteTrigger};
pub(crate) use implementation::{ServerHooks, ServerHooksHandle};

#[cfg(not(test))]
mod implementation {
    use super::*;

    #[derive(Clone, Copy, Default)]
    pub(crate) struct ServerHooks {}

    pub(crate) type ServerHooksHandle = ServerHooks;

    impl ServerHooks {
        #[inline]
        pub(in crate::server) fn shared(&self) -> ServerHooksHandle {
            *self
        }

        #[inline]
        pub(in crate::server) fn request(&self, _: &str, _: &HttpObservation, _: &ConnectionIo) {}

        #[inline]
        pub(in crate::server) fn module_route(&self, _: &str, _: &ActiveModule, _: &Caller) {}

        #[inline]
        pub(in crate::server) async fn health_handler(&self, _: bool) {}

        #[inline]
        pub(in crate::server) async fn before_response_receive(&self) {}

        #[inline]
        pub(in crate::server) fn stream_handoff(&self, _: bool) {}

        #[inline]
        pub(in crate::server) fn close_stream<T>(&self, stream: Option<T>) {
            drop(stream);
        }

        #[inline]
        pub(in crate::server) fn before_accept(&self, _: ServerMode) -> io::Result<()> {
            Ok(())
        }

        #[inline]
        pub(crate) fn spawn_price_worker<T>(
            &self,
            _: &Journal,
            spawn: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<T> {
            spawn()
        }

        #[inline]
        pub(in crate::server) async fn after_upgrade_handoff(
            &self,
            _: &ConnectionAdmission,
            _: bool,
        ) {
        }

        #[inline]
        pub(in crate::server) fn after_report_progress(
            &self,
            _: &Arc<AtomicBool>,
        ) -> impl Fn(i64) + use<> {
            |_| {}
        }

        #[inline]
        pub(in crate::server) fn publish_output<T>(
            &self,
            _: Option<&WriteHook>,
            publish: impl FnOnce() -> T,
        ) -> T {
            publish()
        }

        #[inline]
        pub(crate) fn lock_callback_delegations<'a, T>(
            &self,
            delegations: &'a Mutex<T>,
        ) -> LockResult<MutexGuard<'a, T>> {
            delegations.lock()
        }
    }

    #[derive(Clone, Default)]
    pub(in crate::server) struct WriteHook {}

    impl WriteHook {
        #[inline]
        pub(in crate::server) fn publish_frame(
            &self,
            bytes: Bytes,
            publish: impl FnOnce(Bytes) -> io::Result<()>,
        ) -> io::Result<()> {
            publish(bytes)
        }

        #[inline]
        pub(in crate::server) fn poll(&self, _: &mut Context<'_>, _: &[u8]) -> Poll<()> {
            Poll::Ready(())
        }

        #[inline]
        pub(in crate::server) fn cancel(&self) {}
    }

    const _: () = {
        assert!(size_of::<ServerHooksHandle>() == 0);
        assert!(size_of::<WriteHook>() == 0);
    };
}

#[cfg(test)]
mod implementation {
    use super::*;
    use std::{pin::Pin, sync::TryLockError, task::ready};
    use tokio::sync::oneshot;

    pub(in crate::server) struct RequestProbe {
        pub(in crate::server) path: &'static str,
        pub(in crate::server) id: oneshot::Sender<String>,
        pub(in crate::server) write: Option<WriteProbe>,
    }

    pub(in crate::server) struct WriteProbe {
        pub(in crate::server) trigger: WriteTrigger,
        pub(in crate::server) reached: Option<oneshot::Sender<Option<Vec<u8>>>>,
        pub(in crate::server) release: oneshot::Receiver<()>,
    }

    pub(in crate::server) enum WriteTrigger {
        Bytes(&'static [u8]),
        Output,
        Frame(FrameMatcher),
    }

    type FrameMatcher = Box<dyn Fn(&[u8]) -> bool + Send>;
    pub(in crate::server) struct ReportGate {
        pub(in crate::server) query_paused: oneshot::Sender<Arc<AtomicBool>>,
        pub(in crate::server) release: oneshot::Receiver<()>,
    }

    pub(crate) type ServerHooksHandle = Arc<ServerHooks>;

    type UpgradeHandoffGate = (oneshot::Sender<(usize, usize)>, oneshot::Receiver<()>);
    type HandlerProbe = (oneshot::Sender<()>, oneshot::Receiver<()>);
    type OutputProbe = Box<dyn FnOnce() + Send>;
    type PriceSpawnFailure = Box<dyn FnOnce(&Journal) -> io::Error + Send>;

    #[derive(Default)]
    pub(crate) struct ServerHooks {
        pub(crate) module_routes: Mutex<Vec<(String, u64, String)>>,
        pub(in crate::server) report_gate: Mutex<Option<ReportGate>>,
        pub(crate) upgrade_handoff: Mutex<Option<UpgradeHandoffGate>>,
        pub(in crate::server) request: Mutex<Option<RequestProbe>>,
        pub(in crate::server) health_handler: Mutex<Option<HandlerProbe>>,
        pub(in crate::server) on_stream_handoff: Mutex<Option<OutputProbe>>,
        pub(in crate::server) on_stream_close: Mutex<Option<OutputProbe>>,
        pub(in crate::server) output_queued: Mutex<Option<OutputProbe>>,
        pub(in crate::server) response_receive: Mutex<Option<HandlerProbe>>,
        pub(in crate::server) callback_lock_contended: Mutex<Option<oneshot::Sender<bool>>>,
        pub(crate) price_spawn_failure: Mutex<Option<PriceSpawnFailure>>,
    }

    impl ServerHooks {
        pub(in crate::server) fn shared(self: &Arc<Self>) -> ServerHooksHandle {
            Arc::clone(self)
        }

        pub(in crate::server) fn after_report_progress(
            self: &Arc<Self>,
            cancelled: &Arc<AtomicBool>,
        ) -> impl Fn(i64) + use<> {
            let hooks = Arc::clone(self);
            let cancelled = Arc::clone(cancelled);
            move |completed| {
                if completed == 0 {
                    let gate = hooks.report_gate.lock().unwrap().take();
                    if let Some(gate) = gate {
                        let _ = gate.query_paused.send(cancelled.clone());
                        let _ = gate.release.blocking_recv();
                    }
                }
            }
        }

        pub(in crate::server) async fn after_upgrade_handoff(
            &self,
            admission: &ConnectionAdmission,
            has_request: bool,
        ) {
            if has_request {
                let gate = self.upgrade_handoff.lock().unwrap().take();
                if let Some((reached, resume)) = gate {
                    let _ =
                        reached.send((admission.active_requests(), admission.active_websockets()));
                    let _ = resume.await;
                }
            }
        }

        pub(in crate::server) fn before_accept(&self, mode: ServerMode) -> io::Result<()> {
            if matches!(mode, ServerMode::Recovery)
                && let Some(error) = RECOVERY_ACCEPT_FAILURE.get()
            {
                return Err(error.into());
            }
            Ok(())
        }

        pub(crate) fn spawn_price_worker<T>(
            &self,
            journal: &Journal,
            spawn: impl FnOnce() -> io::Result<T>,
        ) -> io::Result<T> {
            let failure = self.price_spawn_failure.lock().unwrap().take();
            match failure {
                Some(fail) => Err(fail(journal)),
                None => spawn(),
            }
        }

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

        pub(in crate::server) fn module_route(
            &self,
            id: &str,
            module: &ActiveModule,
            caller: &Caller,
        ) {
            self.module_routes.lock().unwrap().push((
                id.to_owned(),
                module.generation,
                caller.id.clone(),
            ));
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

        pub(in crate::server) fn stream_handoff(&self, handed_off: bool) {
            if handed_off {
                let probe = self.on_stream_handoff.lock().unwrap().take();
                if let Some(probe) = probe {
                    probe();
                }
            }
        }

        pub(in crate::server) fn close_stream<T>(&self, stream: Option<T>) {
            if let Some(stream) = stream {
                drop(stream);
                let probe = self.on_stream_close.lock().unwrap().take();
                if let Some(probe) = probe {
                    probe();
                }
            }
        }

        pub(in crate::server) fn publish_output<T>(
            &self,
            write: Option<&WriteHook>,
            publish: impl FnOnce() -> T,
        ) -> T {
            let queued = write.and_then(|write| {
                write.pause_for(|trigger| matches!(trigger, WriteTrigger::Output))
            });
            let result = publish();
            let probe = self.output_queued.lock().unwrap().take();
            if let Some(probe) = probe {
                probe();
            }
            if let Some(queued) = queued {
                let _ = queued.send(None);
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
        pub(in crate::server) fn publish_frame(
            &self,
            bytes: Bytes,
            publish: impl FnOnce(Bytes) -> io::Result<()>,
        ) -> io::Result<()> {
            let queued = self.pause_for(
                |trigger| matches!(trigger, WriteTrigger::Frame(matches) if matches(&bytes)),
            );
            let observed = queued.as_ref().map(|_| bytes.to_vec());
            publish(bytes)?;
            if let Some(queued) = queued {
                let _ = queued.send(observed);
            }
            Ok(())
        }

        fn pause_for(
            &self,
            matches: impl FnOnce(&WriteTrigger) -> bool,
        ) -> Option<oneshot::Sender<Option<Vec<u8>>>> {
            let mut armed = self.0.lock().unwrap();
            let probe = armed.as_mut()?;
            if matches(&probe.trigger) {
                return probe.reached.take();
            }
            None
        }

        pub(in crate::server) fn poll(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<()> {
            let mut armed = self.0.lock().unwrap();
            if let Some(probe) = armed.as_mut()
                && (probe.reached.is_none()
                    || match &probe.trigger {
                        WriteTrigger::Bytes(marker) => {
                            buf.windows(marker.len()).any(|bytes| bytes == *marker)
                        }
                        WriteTrigger::Output | WriteTrigger::Frame(_) => false,
                    })
            {
                if let Some(reached) = probe.reached.take() {
                    let _ = reached.send(None);
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

    thread_local! {
        pub(crate) static RECOVERY_ACCEPT_FAILURE: std::cell::Cell<Option<io::ErrorKind>> =
            const { std::cell::Cell::new(None) };
    }
}
