//! Native credential selection; imported-account rotation stays in the quota owner.
use crate::app::ServerState;
use crate::services::accounts::account_allows_refresh;
use crate::services::accounts::native_auth_document;
use crate::services::quota::refresh_account_serialized;
use emp_codex::account_auth_headers;
use emp_core::ResolvedRoute;
use emp_router::native_http::NativeHttpError;
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
        .configuration
        .vault
        .read_encrypted_json(Path::new(path))
        .map_err(|_| NativeHttpError::router(503, "stored encrypted auth.json is invalid"))?;
    let auth = emp_state::validate_auth_json(&auth)
        .map_err(|error| NativeHttpError::router(503, error.to_string()))?;
    account_auth_headers(&auth)
        .ok_or_else(|| NativeHttpError::router(503, "stored auth.json has no access token"))
}

pub(super) struct ResolvedHeaders {
    pub headers: BTreeMap<String, String>,
    pub native_account: Option<String>,
}

pub(super) fn resolve_headers(
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    stream: bool,
    refresh: bool,
) -> Result<ResolvedHeaders, NativeHttpError> {
    let provider = route.provider.value();
    let selected;
    let mut native_account = None;
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
            if !account_allows_refresh(state, id)
                || account
                    .get("_emp_shared_source")
                    .and_then(Value::as_str)
                    .is_some()
            {
                return Err(NativeHttpError::router(
                    401,
                    "Codex account needs sign-in; sign in again and re-import the account",
                ));
            }
            refresh_account_serialized(state, id)
                .map_err(|error| NativeHttpError::router(503, error.to_string()))?;
        }
        selected = Some(account_headers(state, account)?);
        NativeAuth::Account(selected.as_ref().expect("account headers"))
    } else if provider.get("implicit_native") == Some(&Value::Bool(true)) {
        let document = native_auth_document(&state.backend.accounts.native_auth_path);
        if document
            .as_ref()
            .is_some_and(emp_integration::local_auth::is_local)
        {
            return Err(NativeHttpError::router(
                503,
                "Sign in to a Codex subscription or select an external service model",
            ));
        }
        selected = document.and_then(|auth| {
            emp_state::validate_auth_json(&auth)
                .ok()
                .and_then(|auth| {
                    let headers = account_auth_headers(&auth)?;
                    if state.backend.modules.active(emp_modules::Module::Team) {
                        native_account = Some(emp_state::accounts::account_auth_identity(&auth));
                    }
                    Some(headers)
                })
                .or_else(|| {
                    auth["OPENAI_API_KEY"]
                        .as_str()
                        .filter(|key| !key.is_empty())
                        .map(|key| {
                            BTreeMap::from([("Authorization".to_owned(), format!("Bearer {key}"))])
                        })
                })
        });
        NativeAuth::Implicit(selected.as_ref())
    } else {
        NativeAuth::Forward
    };
    let mut headers = request_headers(auth, incoming, stream)
        .map_err(|error| NativeHttpError::router(error.status(), error.to_string()))?;
    if let Some(hint) = headers.get_mut("x-codex-routing-hint") {
        let suffix = hint.find(';').map(|index| &hint[index..]).unwrap_or("");
        *hint = format!(
            "model={}{suffix}",
            emp_state::config::client_model_id(&route.upstream_model)
        );
    }
    crate::services::usage::identities::remember(state, route, &headers);
    Ok(ResolvedHeaders {
        headers,
        native_account,
    })
}
