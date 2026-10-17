//! Native credential selection; imported-account rotation stays in the quota owner.
use super::*;
use crate::services::accounts::native_auth_document;
use crate::services::quota::refresh_account_serialized;
use emp_codex::account_auth_headers;
use emp_router::native_request::{NativeAuth, request_headers};
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

pub(crate) fn resolve_headers(
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
    let headers = request_headers(auth, incoming, stream)
        .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))?;
    crate::services::usage::identities::remember(state, route, &headers);
    Ok(headers)
}
