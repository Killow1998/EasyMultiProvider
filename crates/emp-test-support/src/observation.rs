use crate::Deadline;
use std::{
    io,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

/// Owns a probe's trigger channel and its worker. Call `begin` with the
/// deadline created at the trigger. The probe gets that same deadline and
/// records the result against it before the caller collects it with `wait`.
/// A result recorded on time can still be collected after the deadline.
/// Dropping the observation disconnects the trigger before dropping the
/// worker. A fixture passed to `with_worker` owns any wakeup and join it needs.
/// Use the supplied deadline for blocking I/O inside a probe.
pub struct Observation<T, W = Worker> {
    trigger: Sender<ProbeRequest<T>>,
    worker: W,
}

impl<T: Send + 'static> Observation<T> {
    pub fn spawn(run: impl FnOnce(Probe<T>) + Send + 'static) -> Self {
        Self::with_worker(|probe| Worker(Some(thread::spawn(move || run(probe)))))
    }
}

impl<T, W> Observation<T, W> {
    pub fn with_worker(start: impl FnOnce(Probe<T>) -> W) -> Self {
        let (trigger, requests) = mpsc::channel();
        let worker = start(Probe(requests));
        Self { trigger, worker }
    }

    pub fn worker(&self) -> &W {
        &self.worker
    }

    pub fn begin(&self, deadline: Deadline, context: &'static str) -> ObservationResult<T> {
        let (send, receive) = mpsc::channel();
        assert!(
            self.trigger
                .send(ProbeRequest {
                    deadline,
                    context,
                    send
                })
                .is_ok(),
            "{context}: observation worker disconnected"
        );
        ObservationResult {
            deadline,
            context,
            receive,
        }
    }
}

pub struct Worker(Option<JoinHandle<()>>);

impl Drop for Worker {
    fn drop(&mut self) {
        let result = self.0.take().unwrap().join();
        if !thread::panicking() {
            result.expect("observation worker panicked");
        }
    }
}

pub struct Probe<T>(Receiver<ProbeRequest<T>>);

impl<T> Probe<T> {
    pub fn next_request(&self) -> Option<ProbeRequest<T>> {
        self.0.recv().ok()
    }

    pub fn observe(&self, read: impl FnOnce(&Deadline) -> T) -> bool {
        let Some(request) = self.next_request() else {
            return false;
        };
        let value = read(request.deadline());
        request.complete(value);
        true
    }
}

pub struct ProbeRequest<T> {
    deadline: Deadline,
    context: &'static str,
    send: Sender<io::Result<T>>,
}

impl<T> ProbeRequest<T> {
    pub fn deadline(&self) -> &Deadline {
        &self.deadline
    }

    pub fn complete(self, value: T) {
        let _ = self.send.send(self.deadline.accept(value, self.context));
    }
}

pub struct ObservationResult<T> {
    deadline: Deadline,
    context: &'static str,
    receive: Receiver<io::Result<T>>,
}

impl<T> ObservationResult<T> {
    pub fn wait(self) -> io::Result<T> {
        self.receive
            .recv_timeout(self.deadline.remaining())
            .map_err(|error| {
                let kind = match error {
                    mpsc::RecvTimeoutError::Timeout => io::ErrorKind::TimedOut,
                    mpsc::RecvTimeoutError::Disconnected => io::ErrorKind::BrokenPipe,
                };
                io::Error::new(kind, format!("{}: {error}", self.context))
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "panic while the probe awaits its trigger")]
    fn panic_disconnects_the_probe_before_joining() {
        let (ready, waiting) = mpsc::channel();
        let _observation = Observation::spawn(move |probe: Probe<()>| {
            ready.send(()).unwrap();
            assert!(!probe.observe(|_| unreachable!()));
        });
        waiting.recv().unwrap();
        panic!("panic while the probe awaits its trigger");
    }
}
