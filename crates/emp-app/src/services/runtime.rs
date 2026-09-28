//! Runtime observations are separate from applied configuration and service health.
use crate::app::ServerState;
use crate::services::catalog::server_catalog;
use emp_codex::runtime_probe::{RuntimeSyncResult, observe};
use emp_integration::runtime::{RuntimeStore, offline_snapshot};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

struct RuntimeData {
    snapshot: Value,
    expected: Vec<String>,
    intent: String,
}

pub(crate) struct RuntimeState {
    store: RuntimeStore,
    data: Mutex<RuntimeData>,
}

impl RuntimeState {
    pub(crate) fn new(path: PathBuf) -> Self {
        let store = RuntimeStore::new(path);
        let data = match store.load() {
            Ok(Some(record)) => RuntimeData {
                snapshot: offline_snapshot(Some(&record), "stale"),
                expected: record.expected_models,
                intent: record.target,
            },
            Ok(None) => RuntimeData {
                snapshot: offline_snapshot(None, "not_checked"),
                expected: Vec::new(),
                intent: "emp".to_owned(),
            },
            Err(error) => RuntimeData {
                snapshot: json!({"state":"unsupported","target":"native","verified":false,
                    "confidence":"stale","detail":error.to_string(),"last_known":null}),
                expected: Vec::new(),
                intent: "emp".to_owned(),
            },
        };
        Self {
            store,
            data: Mutex::new(data),
        }
    }

    pub(crate) fn snapshot(&self) -> Value {
        self.data.lock().expect("runtime state").snapshot.clone()
    }

    fn publish(&self, result: &RuntimeSyncResult, relation: &str) -> Result<(), String> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| "runtime state is unavailable".to_owned())?;
        data.snapshot = json!({
            "state":result.state,"target":result.target,"verified":result.verified,
            "confidence":"live","detail":result.detail,"last_known":null
        });
        self.store
            .save(
                result.state,
                &result.target,
                relation,
                &data.expected,
                result.verified,
                &result.detail,
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

pub(crate) fn sync_runtime(
    state: &ServerState,
    intent: Option<&str>,
    confirmed: bool,
    verify: bool,
    reconcile_search: bool,
) -> Result<RuntimeSyncResult, String> {
    let integration = &state.backend.integration;
    let status = integration
        .manager
        .status()
        .map_err(|error| error.to_string())?;
    if verify && status.relation != "applied" {
        let result = RuntimeSyncResult::new(
            "verification_failed",
            "emp",
            false,
            "EMP configuration is not applied",
        );
        integration.runtime.publish(&result, &status.relation)?;
        return Ok(result);
    }
    let config = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| "configuration is unavailable".to_owned())?
        .clone();
    let catalog = server_catalog(state, &config);
    let catalog_ids = catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| {
            model
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or("list")
                == "list"
        })
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .filter(|id| id.contains('/'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let (target, expected) = {
        let mut data = integration
            .runtime
            .data
            .lock()
            .map_err(|_| "runtime state is unavailable".to_owned())?;
        if let Some(target) = intent {
            if target == "emp" || data.intent != "emp" || data.expected.is_empty() {
                data.expected = catalog_ids.clone();
            }
            data.intent = target.to_owned();
        }
        if verify {
            data.intent = "emp".to_owned();
            if data.expected.is_empty() {
                data.expected = catalog_ids;
            }
        }
        (data.intent.clone(), data.expected.clone())
    };
    if reconcile_search {
        let search_result = if target == "emp" {
            let enabled = config["subscription_search"]["enabled"] == true;
            integration.search.apply(enabled)
        } else {
            integration.search.restore()
        };
        search_result.map_err(|_| "integration state is unavailable".to_owned())?;
    }
    let result = if !verify && !confirmed {
        RuntimeSyncResult::new(
            "reload_required",
            &target,
            false,
            "Confirmation is required before checking the shared Codex backend",
        )
    } else {
        observe(
            &state.backend.accounts.codex_home,
            &expected,
            &target,
            (target == "emp").then_some(&catalog),
        )
    };
    integration.runtime.publish(&result, &status.relation)?;
    Ok(result)
}

pub(crate) fn compatibility_snapshot(state: &ServerState, refresh: bool) -> Value {
    state.backend.integration.inventory.snapshot(refresh)
}

pub(crate) fn helper_binary(state: &ServerState) -> String {
    // Explicit process injection is used by isolated quota fixtures.
    if state.backend.accounts.codex_binary != "codex" {
        return state.backend.accounts.codex_binary.clone();
    }
    state.backend.integration.inventory.executable()
}

pub(crate) fn mark_pending(state: &ServerState, target: &str, detail: &str) -> Result<(), ()> {
    let config = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| ())?
        .clone();
    let catalog = server_catalog(state, &config);
    let expected = catalog["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|model| {
            model
                .get("visibility")
                .and_then(Value::as_str)
                .unwrap_or("list")
                == "list"
        })
        .filter_map(|model| model["slug"].as_str())
        .filter(|slug| slug.contains('/'))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let runtime = &state.backend.integration.runtime;
    let relation = state
        .backend
        .integration
        .manager
        .status()
        .map_err(|_| ())?
        .relation;
    let mut data = runtime.data.lock().map_err(|_| ())?;
    data.intent = target.to_owned();
    data.expected = expected;
    data.snapshot = json!({"state":"reload_required","target":target,"verified":false,"confidence":"pending","detail":detail,"last_known":null});
    runtime
        .store
        .save(
            "reload_required",
            target,
            &relation,
            &data.expected,
            false,
            detail,
        )
        .map_err(|_| ())?;
    Ok(())
}

pub(crate) fn mark_active_pending(state: &ServerState, detail: &str) {
    if state
        .backend
        .integration
        .manager
        .status()
        .is_ok_and(|status| status.state == "active")
    {
        let _ = mark_pending(state, "emp", detail);
    }
}

/// Codex asks for the model list before it swaps the new one in.
const CODEX_SETTLE: Duration = Duration::from_secs(2);
/// A Codex still on an old catalog keeps sending requests; check it at most this often.
const CODEX_RECHECK_GAP: Duration = Duration::from_secs(30);

/// Wakes the runtime watcher when Codex talks to EMP; the page hears the outcome
/// through its event stream instead of asking on a timer.
#[derive(Default)]
pub(crate) struct RuntimeWatch {
    pending: Mutex<bool>,
    wake: Condvar,
    pub(crate) revision: AtomicU64,
}

/// Codex just reached EMP, so it may have restarted with EMP's settings.
pub(crate) fn codex_contacted(state: &ServerState) {
    let watch = &state.backend.integration.watch;
    if let Ok(mut pending) = watch.pending.lock() {
        *pending = true;
        watch.wake.notify_one();
    }
}

pub(crate) fn stop_watch(state: &ServerState) {
    let watch = &state.backend.integration.watch;
    if let Ok(_pending) = watch.pending.lock() {
        watch.wake.notify_all();
    }
}

fn awaiting_codex(state: &ServerState) -> bool {
    let integration = &state.backend.integration;
    let applied = integration
        .manager
        .status()
        .is_ok_and(|status| status.state == "active" && status.relation == "applied");
    let unconflicted = integration
        .startup_conflicts
        .lock()
        .is_ok_and(|conflicts| conflicts.is_empty());
    applied && unconflicted && integration.runtime.snapshot()["state"] != "emp_loaded"
}

pub(crate) fn watch_runtime(state: &ServerState) {
    let watch = &state.backend.integration.watch;
    let mut last_check: Option<Instant> = None;
    loop {
        let Ok(mut pending) = watch.pending.lock() else {
            return;
        };
        while !*pending && !state.shutdown.load(Ordering::Acquire) {
            pending = match watch.wake.wait(pending) {
                Ok(pending) => pending,
                Err(_) => return,
            };
        }
        let gap = last_check.map_or(Duration::ZERO, |at| {
            CODEX_RECHECK_GAP.saturating_sub(at.elapsed())
        });
        let deadline = Instant::now() + CODEX_SETTLE.max(gap);
        loop {
            if state.shutdown.load(Ordering::Acquire) {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            pending = match watch.wake.wait_timeout(pending, deadline - now) {
                Ok((pending, _)) => pending,
                Err(_) => return,
            };
        }
        *pending = false;
        drop(pending);
        if !awaiting_codex(state) {
            continue;
        }
        let before = state.backend.integration.runtime.snapshot()["state"].clone();
        let checked = match state.backend.integration.manager.operation_lock() {
            Ok(_operation) => sync_runtime(state, None, false, true, false).ok(),
            Err(_) => None,
        };
        last_check = Some(Instant::now());
        if checked.is_some_and(|result| before != result.state) {
            watch.revision.fetch_add(1, Ordering::AcqRel);
            if let Ok(_revision) = state.backend.accounts.quota_revision.lock() {
                state.backend.accounts.quota_condition.notify_all();
            }
        }
    }
}
