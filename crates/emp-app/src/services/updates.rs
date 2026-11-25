use crate::services::observation::diagnostics::Diagnostics;
#[cfg(all(test, unix))]
use emp_test_support::{Deadline, TestBudget};
use emp_updater::manager::{Snapshot, UpdateHooks, UpdateManager};
use emp_updater::release::UpdateEndpoints;
use emp_updater::{Result as UpdateResult, UpdateError};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) struct UpdateState {
    pub(crate) manager: UpdateManager,
    gate: Arc<Gate>,
}

struct Gate {
    state: Mutex<GateState>,
    wake: Condvar,
}

struct GateState {
    active: usize,
    draining: bool,
    closed: bool,
}

pub(crate) struct RequestPermit(Arc<Gate>);

impl Drop for RequestPermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.active = state.active.saturating_sub(1);
            self.0.wake.notify_all();
        }
    }
}

impl UpdateState {
    #[expect(
        clippy::too_many_arguments,
        reason = "Update startup binds restart arguments, shutdown signals, diagnostics and module preparation"
    )]
    pub(crate) fn new(
        config: &Path,
        version: &str,
        address: std::net::SocketAddr,
        open_browser: bool,
        installation_rolled_back: bool,
        handoff: impl Fn() -> UpdateResult<()> + Send + Sync + 'static,
        diagnostics: Arc<Diagnostics>,
        modules: Arc<super::modules::Modules>,
    ) -> Self {
        let endpoints = test_endpoints().unwrap_or_default();
        let gate = Arc::new(Gate {
            state: Mutex::new(GateState {
                active: 0,
                draining: false,
                closed: false,
            }),
            wake: Condvar::new(),
        });
        let begin_gate = Arc::clone(&gate);
        let begin_handoff = move || {
            if drain(&begin_gate, Duration::from_secs(300)) {
                Ok(())
            } else {
                Err(UpdateError("requests_busy"))
            }
        };
        let reopen_gate = Arc::clone(&gate);
        let reopen_handoff = move || {
            if let Ok(mut state) = reopen_gate.state.lock() {
                state.draining = state.closed;
                reopen_gate.wake.notify_all();
            }
        };
        let mut restart_args = vec![
            "serve".to_owned(),
            "--config".to_owned(),
            config.to_string_lossy().into_owned(),
            "--host".to_owned(),
            address.ip().to_string(),
            "--port".to_owned(),
            address.port().to_string(),
        ];
        if open_browser {
            restart_args.push("--open-browser".to_owned());
        }
        let executable = std::env::current_exe().unwrap_or_default();
        let manager = UpdateManager::with_startup_hooks(
            executable,
            restart_args,
            version,
            endpoints,
            installation_rolled_back,
            UpdateHooks::new(begin_handoff, reopen_handoff, handoff)
                .with_observer(move |event, fields| {
                    diagnostics.journal.event("info", event, fields)
                })
                .with_module_preparation(move |updater, version| {
                    modules.prepare_update(updater, version)
                }),
        )
        .expect("release update client configuration is valid");
        Self { manager, gate }
    }

    pub(crate) fn enter(&self) -> Option<RequestPermit> {
        let mut state = self.gate.state.lock().ok()?;
        if state.draining {
            return None;
        }
        state.active += 1;
        Some(RequestPermit(Arc::clone(&self.gate)))
    }

    pub(crate) fn close(&self) {
        if let Ok(mut state) = self.gate.state.lock() {
            state.closed = true;
            state.draining = true;
        }
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.manager.snapshot()
    }

    pub(crate) fn start(&self, operation: &str) -> Result<Snapshot, &'static str> {
        self.manager.start(operation).map_err(|error| error.0)
    }
}

#[cfg(all(test, unix))]
impl UpdateState {
    pub(crate) fn drain_for_test(&self, budget: TestBudget) -> impl Future<Output = bool> + use<> {
        let started_at = Instant::now();
        let deadline = Deadline::starting_at(started_at, budget);
        let expires_at = started_at + budget.duration();
        assert!(
            !budget.duration().is_zero(),
            "update drain needs a positive test budget"
        );
        let gate = Arc::clone(&self.gate);
        async move {
            let task = tokio::task::spawn_blocking(move || {
                if deadline.is_expired() {
                    return false;
                }
                let Ok(state) = gate.state.lock() else {
                    return false;
                };
                !deadline.is_expired() && drain_requests(&gate, state, expires_at)
            });
            match tokio::time::timeout_at(expires_at.into(), task).await {
                Ok(drained) => drained.unwrap() && !deadline.is_expired(),
                Err(_) => false,
            }
        }
    }

    /// Use only while the test holds an in-flight request open.
    pub(crate) fn drain_blocked_for_test(&self) -> bool {
        !drain(&self.gate, Duration::ZERO)
    }

    pub(crate) fn active_for_test(&self) -> usize {
        self.gate.state.lock().unwrap().active
    }
}

fn test_endpoints() -> Option<UpdateEndpoints> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let repository = std::env::var("EMP_UPDATE_TEST_REPOSITORY_URL").ok()?;
    let api = std::env::var("EMP_UPDATE_TEST_API_URL").ok()?;
    Some(UpdateEndpoints::for_source(repository, api))
}

fn drain(gate: &Gate, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let Ok(state) = gate.state.lock() else {
        return false;
    };
    drain_requests(gate, state, deadline)
}

fn drain_requests(gate: &Gate, mut state: MutexGuard<'_, GateState>, deadline: Instant) -> bool {
    state.draining = true;
    while state.active > 0 {
        let now = Instant::now();
        if now >= deadline {
            state.draining = state.closed;
            gate.wake.notify_all();
            return false;
        }
        let Ok((next, wait)) = gate.wake.wait_timeout(state, deadline - now) else {
            return false;
        };
        state = next;
        if wait.timed_out() && state.active > 0 {
            state.draining = state.closed;
            gate.wake.notify_all();
            return false;
        }
    }
    true
}
