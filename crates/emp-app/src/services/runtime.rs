//! Runtime observations are separate from applied configuration and service health.
use crate::app::ServerState;
use crate::services::catalog::server_catalog;
use emp_codex::runtime_probe::{RuntimeSyncResult, observe};
use emp_integration::runtime::{RuntimeStore, offline_snapshot};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

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

pub(crate) fn runtime_preferences(state: &ServerState) -> Value {
    state
        .backend
        .configuration
        .config
        .lock()
        .ok()
        .and_then(|config| config.get("codex_runtime_sources").cloned())
        .unwrap_or(json!(["auto"]))
}

pub(crate) fn compatibility_snapshot(state: &ServerState, refresh: bool) -> Value {
    state
        .backend
        .integration
        .inventory
        .snapshot(&runtime_preferences(state), refresh)
}

pub(crate) fn helper_binary(state: &ServerState) -> String {
    // Explicit process injection is used by isolated quota fixtures.
    if state.backend.accounts.codex_binary != "codex" {
        return state.backend.accounts.codex_binary.clone();
    }
    state
        .backend
        .integration
        .inventory
        .executable(&runtime_preferences(state))
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
