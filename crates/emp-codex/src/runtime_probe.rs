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
            detail: "Codex local control access was denied",
        },
        503 => ProbeError {
            kind: "unavailable",
            detail: "The shared Codex backend is unavailable",
        },
        504 => ProbeError {
            kind: "unavailable",
            detail: "The shared Codex backend did not answer the control request",
        },
        500 => ProbeError {
            kind: "command",
            detail: "Codex local control request failed",
        },
        _ => ProbeError {
            kind: "malformed",
            detail: "Codex returned an invalid local control response",
        },
    }
}

fn response(
    socket: &mut ClientWebSocket,
    id: u64,
    rejected_detail: &'static str,
) -> Result<Value, ProbeError> {
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
                kind: if message["error"]["code"].as_i64() == Some(-32601) {
                    "unsupported"
                } else {
                    "rejected"
                },
                detail: rejected_detail,
            });
        }
        return Ok(message);
    }
    Err(ProbeError {
        kind: "malformed",
        detail: "Codex did not return a usable model catalog",
    })
}

fn initialize(socket: &mut ClientWebSocket) -> Result<(), ProbeError> {
    socket
        .send_json(&json!({"id":1,"method":"initialize","params":{
            "clientInfo":{"name":"easy-multi-provider","title":"EMP","version":env!("CARGO_PKG_VERSION")},
            "capabilities":null
        }}))
        .map_err(transport_error)?;
    response(socket, 1, "Codex rejected the control connection handshake")?;
    socket
        .send_json(&json!({"method":"initialized"}))
        .map_err(transport_error)
}

fn model_list(home: &Path) -> Result<Vec<Value>, ProbeError> {
    let path = home.join("app-server-control/app-server-control.sock");
    let mut socket =
        ClientWebSocket::connect_local(&path, Duration::from_secs(15)).map_err(transport_error)?;
    let result = (|| {
        initialize(&mut socket)?;
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
            let reply = response(&mut socket, id, "Codex rejected the model catalog query")?;
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
            "catalog_unverified",
            target,
            false,
            "The Codex control interface is unavailable; settings are saved, but the shared model catalog is not yet verified",
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
            "Codex's visible model IDs, names, descriptions and visibility match EMP's current catalog; request routing is not exposed by this check".into(),
        );
    }
    let expected = expected
        .iter()
        .filter(|id| !id.is_empty())
        .collect::<BTreeSet<_>>();
    if expected.is_empty() {
        return make("catalog_unverified",false,"Without distinct EMP model IDs, the shared model list cannot distinguish EMP from native settings; request routing is not exposed by this check".into());
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
        make("emp_loaded",true,"The shared Codex model list exposes all expected EMP model IDs; provider routing is not exposed by this check".into())
    } else {
        if entries.is_empty() {
            return make(
                "catalog_unverified",
                false,
                "Codex returned an empty model list, so the native catalog cannot be verified"
                    .into(),
            );
        }
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
        make("native_loaded",false,"The shared Codex model list exposes no previous EMP-specific model IDs; this check does not verify restored provider routing or other native settings".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_control_socket_does_not_imply_the_app_is_stopped() {
        let home = tempfile::tempdir().unwrap();
        for target in ["emp", "native"] {
            let result = observe(home.path(), &["emp/model-a".to_owned()], target, None);
            assert_eq!(result.state, "catalog_unverified");
            assert_eq!(result.target, target);
            assert!(!result.verified);
            assert!(result.observed_models.is_empty());
            assert_eq!(
                result.detail,
                "The Codex control interface is unavailable; settings are saved, but the shared model catalog is not yet verified"
            );
        }
    }

    #[test]
    fn native_catalog_is_unverified_when_the_shared_list_is_empty() {
        let result = validate_models(&[], &["emp/model-a".to_owned()], "native", None);

        assert_eq!(result.state, "catalog_unverified");
        assert!(!result.verified);
        assert!(result.detail.contains("empty model list"));
    }

    #[test]
    fn matching_model_catalog_does_not_claim_request_routing() {
        let result = validate_models(
            &[json!({"id":"emp/model-a","displayName":"Model A","description":""})],
            &["emp/model-a".to_owned()],
            "emp",
            None,
        );

        assert_eq!(result.state, "emp_loaded");
        assert!(result.verified);
        assert!(result.detail.contains("routing is not exposed"));
    }

    #[test]
    fn native_model_list_without_emp_ids_does_not_claim_restoration() {
        let result = validate_models(
            &[json!({"id":"gpt-5.5"})],
            &["emp/model-a".to_owned()],
            "native",
            None,
        );

        assert_eq!(result.state, "native_loaded");
        assert!(!result.verified);
        assert!(result.detail.contains("does not verify restored"));
    }
}
