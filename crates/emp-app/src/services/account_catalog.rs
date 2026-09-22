//! Refresh one subscription's entitlements, checking ownership before persistence.
use crate::app::ServerState;
use crate::http::response::{json_error_response, status_text};
use crate::services::accounts::{account_catalog_headers, native_auth_document};
use crate::services::failures::router_error_response;
use serde_json::{Value, json};
use std::path::PathBuf;
fn invalid(message: &str) -> Vec<u8> {
    json_error_response(400, status_text(400), message, None, &[])
}
fn internal() -> Vec<u8> {
    json_error_response(500, status_text(500), "internal server error", None, &[])
}
fn account_path(account: &Value) -> Option<PathBuf> {
    Some(
        PathBuf::from(account["auth_file"].as_str().filter(|s| !s.is_empty())?)
            .parent()?
            .join("models_cache.json"),
    )
}
pub(crate) fn refresh(state: &ServerState, id: &str) -> Result<Value, Vec<u8>> {
    let config = state
        .backend
        .configuration
        .config
        .lock()
        .map_err(|_| internal())?
        .clone();
    let account = if id == "@native" {
        None
    } else {
        Some(
            config["accounts"]
                .as_array()
                .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
                .filter(|account| account_path(account).is_some())
                .ok_or_else(|| invalid("Subscription account is unavailable"))?,
        )
    };
    let base = config["codex_base_url"].as_str().unwrap_or("");
    let headers = match account {
        Some(account) => account_catalog_headers(
            account.as_object().unwrap(),
            &state.backend.configuration.vault,
        ),
        None => native_auth_document(&state.backend.accounts.native_auth_path)
            .and_then(|auth| emp_codex::account_auth_headers(&auth)),
    }
    .ok_or_else(|| invalid("Subscription credentials are unavailable"))?;
    let owner = emp_codex::native_catalog_owner(&headers);
    let path = account
        .and_then(account_path)
        .unwrap_or_else(|| emp_codex::native_catalog_path(config.as_object().unwrap()));
    let mut catalog = state
        .backend
        .transport
        .runtime
        .block_on(emp_router::subscription_catalog::fetch(
            &state.backend.transport.client,
            base,
            &headers,
        ))
        .map_err(router_error_response)?;
    {
        let current = state
            .backend
            .configuration
            .config
            .lock()
            .map_err(|_| internal())?;
        if current["codex_base_url"] != base {
            return Err(invalid(
                "Subscription backend changed during model refresh; retry",
            ));
        }
        if account.is_some() {
            let current_account = current["accounts"]
                .as_array()
                .and_then(|accounts| accounts.iter().find(|account| account["id"] == id));
            let current_owner = current_account
                .and_then(Value::as_object)
                .and_then(|account| {
                    account_catalog_headers(account, &state.backend.configuration.vault)
                })
                .map(|headers| emp_codex::native_catalog_owner(&headers));
            if current_account.and_then(account_path).as_ref() != Some(&path)
                || current_owner.as_deref() != Some(&owner)
            {
                return Err(invalid("Subscription changed during model refresh; retry"));
            }
            catalog["account_owner"] = json!(owner);
            catalog["base_url"] = json!(base);
        } else {
            let current_headers = native_auth_document(&state.backend.accounts.native_auth_path)
                .and_then(|auth| emp_codex::account_auth_headers(&auth));
            if emp_codex::native_catalog_path(current.as_object().unwrap()) != path
                || current_headers.as_ref() != Some(&headers)
            {
                return Err(invalid("Native login changed during model refresh; retry"));
            }
        }
        emp_state::filesystem::write_catalog_json(&path, &catalog).map_err(|_| internal())?;
        if account.is_none() {
            emp_codex::preserve_native_catalog(&config).map_err(|_| internal())?;
        }
    }
    crate::services::catalog::refresh_catalog(state).map_err(|_| internal())?;
    Ok(json!({"models":emp_codex::management_views::subscription_model_options(&catalog)}))
}
