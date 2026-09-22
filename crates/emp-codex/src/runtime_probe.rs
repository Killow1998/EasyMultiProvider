//! Read the shared Codex model catalog over its local control WebSocket.
use emp_transport::ClientWebSocket;
use emp_transport::ClientWebSocketError;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct RuntimeSyncResult {
    pub state: &'static str,
    pub target: String,
    pub verified: bool,
    pub detail: String,
    pub observed_models: Vec<String>,
}

impl RuntimeSyncResult {
    pub fn new(
        state: &'static str,
        target: &str,
        verified: bool,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            state,
            target: target.to_owned(),
            verified,
            detail: detail.into(),
            observed_models: Vec::new(),
        }
    }
}

struct ProbeError {
    kind: &'static str,
    detail: &'static str,
}

fn transport_error(error: ClientWebSocketError) -> ProbeError {
    match error.status() {
        403 => ProbeError {
            kind: "permission",
            detail: "Codex model catalog access was denied",
        },
        503 => ProbeError {
            kind: "unavailable",
            detail: "The shared Codex backend is unavailable",
        },
        504 => ProbeError {
            kind: "unavailable",
            detail: "The shared Codex backend did not answer the catalog probe",
        },
        500 => ProbeError {
            kind: "command",
            detail: "Codex model catalog query failed",
        },
        _ => ProbeError {
            kind: "malformed",
            detail: "Codex returned an invalid model catalog",
        },
    }
}

fn response(socket: &mut ClientWebSocket, id: u64) -> Result<Value, ProbeError> {
    for _ in 0..100 {
        let Some(message) = socket.receive_value().map_err(transport_error)? else {
            return Err(ProbeError {
                kind: "unavailable",
                detail: "The shared Codex backend closed the catalog probe",
            });
        };
        if message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if message.get("error").is_some_and(|value| !value.is_null()) {
            return Err(ProbeError {
                kind: "permission",
                detail: "Codex rejected the read-only model catalog query",
            });
        }
        return Ok(message);
    }
    Err(ProbeError {
        kind: "malformed",
        detail: "Codex did not return a usable model catalog",
    })
}

fn model_list(home: &Path) -> Result<Vec<Value>, ProbeError> {
    let path = home.join("app-server-control/app-server-control.sock");
    let mut socket =
        ClientWebSocket::connect_local(&path, Duration::from_secs(15)).map_err(transport_error)?;
    let result = (|| {
        socket.send_json(&json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"easy-multi-provider","title":"EMP","version":env!("CARGO_PKG_VERSION")},
            "capabilities":null
        }})).map_err(transport_error)?;
        response(&mut socket, 1)?;
        socket
            .send_json(&json!({"method":"initialized"}))
            .map_err(transport_error)?;
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        for page in 0..20 {
            let mut params = json!({"includeHidden":false,"limit":1000});
            if let Some(cursor) = &cursor {
                params["cursor"] = json!(cursor);
            }
            let id = page + 2;
            socket
                .send_json(&json!({"id":id,"method":"model/list","params":params}))
                .map_err(transport_error)?;
            let reply = response(&mut socket, id)?;
            let result = reply
                .get("result")
                .filter(|value| value.is_object())
                .ok_or(ProbeError {
                    kind: "malformed",
                    detail: "Codex returned an invalid model catalog",
                })?;
            let data = result
                .get("data")
                .and_then(Value::as_array)
                .ok_or(ProbeError {
                    kind: "malformed",
                    detail: "Codex returned an invalid model catalog",
                })?;
            models.extend(
                data.iter()
                    .filter(|value| value.get("id").is_some_and(Value::is_string))
                    .cloned(),
            );
            if models.len() > 20_000 {
                return Err(ProbeError {
                    kind: "malformed",
                    detail: "Codex model catalog is too large",
                });
            }
            match result.get("nextCursor") {
                None | Some(Value::Null) => return Ok(models),
                Some(Value::String(value)) if value.is_empty() => return Ok(models),
                Some(Value::String(value))
                    if value.chars().count() <= 1024 && seen.insert(value.clone()) =>
                {
                    cursor = Some(value.clone())
                }
                _ => {
                    return Err(ProbeError {
                        kind: "malformed",
                        detail: "Codex returned an invalid model cursor",
                    });
                }
            }
        }
        Err(ProbeError {
            kind: "malformed",
            detail: "Codex model catalog pagination exceeded its limit",
        })
    })();
    socket.close();
    result
}

pub fn observe(
    home: &Path,
    expected: &[String],
    target: &str,
    catalog: Option<&Value>,
) -> RuntimeSyncResult {
    if !matches!(target, "emp" | "native") {
        return RuntimeSyncResult::new("unsupported", target, false, "Unknown runtime target");
    }
    if expected.is_empty() && catalog.is_none() {
        return validate_models(&[], expected, target, catalog);
    }
    match model_list(home) {
        Ok(models) => validate_models(&models, expected, target, catalog),
        Err(error) if error.kind == "unavailable" => RuntimeSyncResult::new(
            "stopped_waiting_for_start",
            target,
            false,
            "The shared Codex backend is unavailable; configuration is saved and will load when its owner starts it",
        ),
        Err(error) => RuntimeSyncResult::new(
            if error.kind == "unsupported" {
                "unsupported"
            } else {
                "verification_failed"
            },
            target,
            false,
            error.detail,
        ),
    }
}

fn validate_models(
    models: &[Value],
    expected: &[String],
    target: &str,
    catalog: Option<&Value>,
) -> RuntimeSyncResult {
    let mut observed = Vec::new();
    let mut entries = BTreeMap::new();
    for model in models {
        if let Some(id) = model.get("id").and_then(Value::as_str) {
            if !entries.contains_key(id) {
                observed.push(id.to_owned());
            }
            entries.insert(id.to_owned(), model);
        }
    }
    let make = |state, verified, detail: String| {
        let mut result = RuntimeSyncResult::new(state, target, verified, detail);
        result.observed_models = observed.clone();
        result
    };
    if target == "emp"
        && let Some(catalog) = catalog
    {
        let visible = catalog
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|model| {
                model
                    .get("visibility")
                    .and_then(Value::as_str)
                    .unwrap_or("list")
                    == "list"
            })
            .filter_map(|model| {
                model
                    .get("slug")
                    .and_then(Value::as_str)
                    .map(|id| (id.to_owned(), model))
            })
            .collect::<BTreeMap<_, _>>();
        if visible.keys().ne(entries.keys()) {
            return make(
                "reload_required",
                false,
                "The running Codex model list has not refreshed yet".into(),
            );
        }
        if visible.iter().any(|(id, model)| {
            let entry = entries[id];
            entry.get("displayName").and_then(Value::as_str)
                != Some(
                    model
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(id),
                )
                || entry.get("description").and_then(Value::as_str)
                    != Some(
                        model
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
        }) {
            return make(
                "reload_required",
                false,
                "The running Codex model display settings have not refreshed yet".into(),
            );
        }
        return make(
            "emp_loaded",
            true,
            "Codex has loaded the current model names, descriptions and visibility".into(),
        );
    }
    let expected = expected
        .iter()
        .filter(|id| !id.is_empty())
        .collect::<BTreeSet<_>>();
    if expected.is_empty() {
        return make("catalog_unverified",false,"Catalog settings are saved. Without distinct EMP model IDs, this check cannot verify native model visibility or display names. Restart active Codex clients safely, then check their model picker.".into());
    }
    if target == "emp" {
        let missing = expected
            .iter()
            .filter(|id| !entries.contains_key(id.as_str()))
            .count();
        if missing > 0 {
            return make(
                "reload_required",
                false,
                format!(
                    "Codex is missing {missing} expected EMP model(s); wait for its owner to refresh the catalog, or restart Codex safely"
                ),
            );
        }
        make("emp_loaded",true,"The shared Codex backend exposes all expected EMP model IDs; other startup settings are not verified".into())
    } else {
        let remaining = expected
            .iter()
            .filter(|id| entries.contains_key(id.as_str()))
            .count();
        if remaining > 0 {
            return make(
                "reload_required",
                false,
                format!("Codex still exposes {remaining} EMP model(s)"),
            );
        }
        make("native_loaded",true,"The shared Codex backend exposes no expected EMP model IDs; other startup settings are not verified".into())
    }
}
