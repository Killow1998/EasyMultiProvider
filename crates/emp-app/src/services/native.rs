//! Native credential selection and request forwarding.

use crate::app::ServerState;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::services::accounts::native_auth_document;
use crate::services::catalog::response_catalog_etag;
use crate::services::quota::refresh_account_serialized;
use emp_codex::account_auth_headers;
use emp_core::ResolvedRoute;
use emp_router::ProjectionIds;
use emp_router::native_http::NativeHttpError;
use emp_router::native_http::NativeRouter;
use emp_router::native_http::NativeStream;
use emp_router::native_http::NativeWebSocketPlan;
use emp_router::native_request::NativeAuth;
use emp_router::native_request::request_headers;
use serde_json::Map;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

fn account_headers(
    state: &ServerState,
    account: &Map<String, Value>,
) -> Result<BTreeMap<String, String>, NativeHttpError> {
    let path = account
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            NativeHttpError::router(
                503,
                format!(
                    "credentials are not configured for account: {}",
                    account
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                ),
            )
        })?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(NativeHttpError::router(
            503,
            "stored account credentials cannot be a symlink",
        ));
    }
    let auth = state
        .backend
        .configuration
        .vault
        .read_encrypted_json(Path::new(path))
        .map_err(|_| NativeHttpError::router(503, "stored encrypted auth.json is invalid"))?;
    let auth = emp_state::validate_auth_json(&auth)
        .map_err(|error| NativeHttpError::router(503, error.to_string()))?;
    account_auth_headers(&auth)
        .ok_or_else(|| NativeHttpError::router(503, "stored auth.json has no access token"))
}

fn resolve_headers(
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    stream: bool,
    refresh: bool,
) -> Result<BTreeMap<String, String>, NativeHttpError> {
    let provider = route.provider.value();
    let selected;
    let auth = if provider.get("auth_mode").and_then(Value::as_str) == Some("account") {
        let account = provider
            .get("account")
            .and_then(Value::as_object)
            .ok_or_else(|| NativeHttpError::router(503, "stored encrypted auth.json is invalid"))?;
        if refresh {
            let id = account
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            refresh_account_serialized(state, id)
                .map_err(|error| NativeHttpError::router(503, error.to_string()))?;
        }
        selected = Some(account_headers(state, account)?);
        NativeAuth::Account(selected.as_ref().expect("account headers"))
    } else if provider.get("implicit_native") == Some(&Value::Bool(true)) {
        selected = native_auth_document(&state.backend.accounts.native_auth_path)
            .and_then(|auth| emp_state::validate_auth_json(&auth).ok())
            .and_then(|auth| account_auth_headers(&auth));
        NativeAuth::Implicit(selected.as_ref())
    } else {
        NativeAuth::Forward
    };
    request_headers(auth, incoming, stream)
        .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))
}

fn plaintext_collaboration(config: &Value) -> bool {
    config
        .get("providers")
        .and_then(Value::as_array)
        .is_some_and(|providers| {
            providers.iter().any(|provider| {
                provider.get("auth_mode").and_then(Value::as_str) == Some("api_key")
            })
        })
}

fn error_response(mut error: NativeHttpError) -> Vec<u8> {
    error.headers.remove("x-models-etag");
    let headers = error
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    response(
        &format!("HTTP/1.1 {} {}", error.status, status_text(error.status)),
        "application/json",
        &serde_json::to_vec(&error.body).expect("safe native error JSON"),
        &headers,
    )
}

fn replace_catalog_etag(state: &ServerState, headers: &mut BTreeMap<String, String>) {
    headers.remove("x-models-etag");
    if let Some(etag) = response_catalog_etag(state) {
        headers.insert("X-Models-Etag".to_owned(), etag);
    }
}

pub(crate) fn open_stream(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<NativeStream, Vec<u8>> {
    open_stream_result(state, route, config, body, incoming, ids).map_err(error_response)
}

pub(crate) fn open_stream_result(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<NativeStream, NativeHttpError> {
    let started = std::time::Instant::now();
    let router = NativeRouter::new(&state.backend.transport.client);
    let mut usage_owner = String::new();
    let result = state.backend.transport.runtime.block_on(router.open_stream(
        route,
        body,
        plaintext_collaboration(config),
        ids,
        |refresh| {
            let headers = resolve_headers(state, route, incoming, true, refresh)?;
            usage_owner = emp_state::usage::account_owner(&headers);
            Ok(headers)
        },
    ));
    match result {
        Ok(mut stream) => {
            stream.usage_owner = Some(usage_owner);
            replace_catalog_etag(state, &mut stream.headers);
            Ok(stream)
        }
        Err(error) => {
            let mut observation = crate::services::observation::Observation::new(
                state,
                route,
                &Value::Object(body.clone()),
                incoming,
                Some(&usage_owner),
                "responses",
            )
            .started_at(started);
            observation.native_error(&error);
            Err(error)
        }
    }
}

pub(crate) fn complete(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Vec<u8> {
    let started = std::time::Instant::now();
    let router = NativeRouter::new(&state.backend.transport.client);
    let mut usage_owner = String::new();
    let result = state
        .backend
        .transport
        .runtime
        .block_on(router.execute_complete(
            route,
            body,
            plaintext_collaboration(config),
            true,
            |refresh| {
                let headers = resolve_headers(state, route, incoming, false, refresh)?;
                usage_owner = emp_state::usage::account_owner(&headers);
                Ok(headers)
            },
        ));
    let mut usage = crate::services::observation::Observation::new(
        state,
        route,
        &Value::Object(body.clone()),
        incoming,
        Some(&usage_owner),
        "responses",
    )
    .started_at(started);
    match result {
        Ok(mut result) => {
            usage.http_status(result.status);
            if let Ok(value) = serde_json::from_slice::<Value>(&result.body) {
                usage.observe(&value);
                crate::services::context::record_event(
                    state,
                    route,
                    &Value::Object(body.clone()),
                    &serde_json::json!({"type":format!("response.{}",value["status"].as_str().unwrap_or("unknown")),"response":value}),
                );
            }
            // EMP's current catalog identity supersedes an upstream's catalog.
            replace_catalog_etag(state, &mut result.headers);
            let headers = result
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            response(
                &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
                &result.content_type,
                &result.body,
                &headers,
            )
        }
        Err(error) => {
            usage.native_error(&error);
            crate::services::context::record_event(
                state,
                route,
                &Value::Object(body.clone()),
                &serde_json::json!({"type":"error","error":error.body["error"]}),
            );
            error_response(error)
        }
    }
}

pub(crate) fn compact(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Vec<u8> {
    let started = std::time::Instant::now();
    let router = NativeRouter::new(&state.backend.transport.client);
    let mut usage_owner = String::new();
    let result = state
        .backend
        .transport
        .runtime
        .block_on(router.execute_compact(
            route,
            body,
            plaintext_collaboration(config),
            |refresh| {
                let headers = resolve_headers(state, route, incoming, false, refresh)?;
                usage_owner = emp_state::usage::account_owner(&headers);
                Ok(headers)
            },
        ));
    let mut usage = crate::services::observation::Observation::new(
        state,
        route,
        &Value::Object(body.clone()),
        incoming,
        Some(&usage_owner),
        "compact",
    )
    .started_at(started);
    match result {
        Ok(mut result) => {
            usage.http_status(result.status);
            if let Ok(value) = serde_json::from_slice::<Value>(&result.body) {
                usage.observe(&value);
            }
            replace_catalog_etag(state, &mut result.headers);
            let headers = result
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect::<Vec<_>>();
            response(
                &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
                &result.content_type,
                &result.body,
                &headers,
            )
        }
        Err(error) => {
            usage.native_error(&error);
            error_response(error)
        }
    }
}

pub(crate) fn websocket_plan(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Result<NativeWebSocketPlan, NativeHttpError> {
    let headers = resolve_headers(state, route, incoming, true, false)?;
    NativeRouter::new(&state.backend.transport.client).prepare_websocket(
        route,
        body,
        plaintext_collaboration(config),
        headers,
    )
}
