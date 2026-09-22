//! Application-owned credentials for native Responses. Python remains the
//! production service; this completes the Rust non-streaming HTTP path.

use super::{ServerState, native_auth_document, refresh_account_serialized, response, status_text};
use emp_codex::account_auth_headers;
use emp_core::ResolvedRoute;
use emp_router::ProjectionIds;
use emp_router::native_http::{NativeHttpError, NativeRouter, NativeStream, NativeWebSocketPlan};
use emp_router::native_request::{NativeAuth, request_headers};
use serde_json::{Map, Value};
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
        selected = native_auth_document(&state.backend.native_auth_path)
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
    if let Some(etag) = super::catalog_api::response_catalog_etag(state) {
        headers.insert("X-Models-Etag".to_owned(), etag);
    }
}

pub(super) fn open_stream(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<NativeStream, Vec<u8>> {
    open_stream_result(state, route, config, body, incoming, ids).map_err(error_response)
}

pub(super) fn open_stream_result(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<NativeStream, NativeHttpError> {
    let router = NativeRouter::new(&state.backend.client);
    let result = state.backend.runtime.block_on(router.open_stream(
        route,
        body,
        plaintext_collaboration(config),
        ids,
        |refresh| resolve_headers(state, route, incoming, true, refresh),
    ));
    match result {
        Ok(mut stream) => {
            replace_catalog_etag(state, &mut stream.headers);
            Ok(stream)
        }
        Err(error) => Err(error),
    }
}

pub(super) fn complete(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Vec<u8> {
    let router = NativeRouter::new(&state.backend.client);
    match state.backend.runtime.block_on(router.execute_complete(
        route,
        body,
        plaintext_collaboration(config),
        true,
        |refresh| resolve_headers(state, route, incoming, false, refresh),
    )) {
        Ok(mut result) => {
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
        Err(error) => error_response(error),
    }
}

pub(super) fn compact(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Vec<u8> {
    let router = NativeRouter::new(&state.backend.client);
    match state.backend.runtime.block_on(router.execute_compact(
        route,
        body,
        plaintext_collaboration(config),
        |refresh| resolve_headers(state, route, incoming, false, refresh),
    )) {
        Ok(mut result) => {
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
        Err(error) => error_response(error),
    }
}

pub(super) fn websocket_plan(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Result<NativeWebSocketPlan, NativeHttpError> {
    let headers = resolve_headers(state, route, incoming, true, false)?;
    NativeRouter::new(&state.backend.client).prepare_websocket(
        route,
        body,
        plaintext_collaboration(config),
        headers,
    )
}
