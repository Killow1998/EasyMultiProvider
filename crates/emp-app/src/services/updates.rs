//! Update request admission and orderly application handoff.
use emp_state::update::manager::{Snapshot, UpdateManager};
use emp_state::update::release::UpdateEndpoints;
use emp_state::update::{Result as UpdateResult, UpdateError};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
    pub(crate) fn new(
        config: &Path,
        version: &str,
        address: std::net::SocketAddr,
        open_browser: bool,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let endpoints = test_endpoints().unwrap_or_default();
        Self::with_endpoints(config, version, address, open_browser, shutdown, endpoints)
    }

    pub(crate) fn with_endpoints(
        _config: &Path,
        version: &str,
        address: std::net::SocketAddr,
        open_browser: bool,
        shutdown: Arc<AtomicBool>,
        endpoints: UpdateEndpoints,
    ) -> Self {
        let gate = Arc::new(Gate {
            state: Mutex::new(GateState {
                active: 0,
                draining: false,
            }),
            wake: Condvar::new(),
        });
        let handoff_gate = Arc::clone(&gate);
        let handoff_shutdown = Arc::clone(&shutdown);
        let handoff = move || -> UpdateResult<()> {
            if !drain(&handoff_gate, Duration::from_secs(300)) {
                return Err(UpdateError("requests_busy"));
            }
            handoff_shutdown.store(true, Ordering::Release);
            Ok(())
        };
        let mut restart_args = vec![
            "serve".to_owned(),
            "--config".to_owned(),
            _config.to_string_lossy().into_owned(),
            "--host".to_owned(),
            address.ip().to_string(),
            "--port".to_owned(),
            address.port().to_string(),
        ];
        if open_browser {
            restart_args.push("--open-browser".to_owned());
        }
        let executable = std::env::current_exe().unwrap_or_default();
        let manager =
            UpdateManager::with_endpoints(executable, restart_args, version, endpoints, handoff)
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

    pub(crate) fn snapshot(&self) -> Snapshot {
        self.manager.snapshot()
    }

    pub(crate) fn start(&self, operation: &str) -> Result<Snapshot, &'static str> {
        self.manager.start(operation).map_err(|error| error.0)
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
    let Ok(mut state) = gate.state.lock() else {
        return false;
    };
    state.draining = true;
    while state.active > 0 {
        let now = Instant::now();
        if now >= deadline {
            state.draining = false;
            gate.wake.notify_all();
            return false;
        }
        let Ok((next, wait)) = gate.wake.wait_timeout(state, deadline - now) else {
            return false;
        };
        state = next;
        if wait.timed_out() && state.active > 0 {
            state.draining = false;
            gate.wake.notify_all();
            return false;
        }
    }
    true
}
