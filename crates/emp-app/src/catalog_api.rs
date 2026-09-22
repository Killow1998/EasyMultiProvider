//! Authenticated model discovery, selected-model persistence, and Codex catalogs.
use super::{
    Request, ServerState, account_catalog_headers, body_error_response, cross_origin_response,
    json_error_response, native_auth_document, percent_decode, query_values, read_json_body,
    response, router_error_response, same_origin, status_text, unauthorized_response,
};
use emp_codex::{
    account_catalog, load_native_catalog, merged_catalog::build_catalog, preserve_native_catalog,
};
use emp_router::discovery::discover_models;
use emp_state::{
    ConfigError, discovery_merge::merge_selected_models, filesystem::write_catalog_json,
    load_configuration, observed_at_now, provider_api_key, same_account_auth,
    save_configuration_in_transaction, validate_auth_json, with_file_transaction,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};

pub(super) fn management_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    let cookie = request.session_cookie();
    if !state.sessions.contains(cookie.as_deref(), now) {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if request.raw_path() == "/api/catalog/refresh" {
        let config = match state.backend.config.lock() {
            Ok(config) => config,
            Err(_) => return internal_error(),
        };
        let catalog = server_catalog(state, &config);
        let path = generated_catalog_path(state);
        if write_catalog_json(&path, &catalog).is_err() {
            return internal_error();
        }
        return json_response(
            &json!({"status":"ok","catalog_path":path,"model_count":catalog["models"].as_array().map_or(0,Vec::len)}),
        );
    }
    let Some(provider_id) = body
        .get("provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return config_error("provider is required");
    };
    let mut provider = {
        let config = match state.backend.config.lock() {
            Ok(config) => config,
            Err(_) => return internal_error(),
        };
        let Some(provider) = config
            .get("providers")
            .and_then(Value::as_array)
            .and_then(|providers| {
                providers.iter().find(|provider| {
                    provider.get("id").and_then(Value::as_str) == Some(provider_id)
                })
            })
            .filter(|provider| provider.get("enabled") != Some(&Value::Bool(false)))
        else {
            return config_error(&format!("provider is missing or disabled: {provider_id}"));
        };
        provider.clone()
    };
    provider["api_key"] = json!(provider_api_key(&provider, &state.backend.vault));
    let discovered = {
        let _guard = match state.backend.discovery_lock.lock() {
            Ok(guard) => guard,
            Err(_) => return internal_error(),
        };
        match state.backend.runtime.block_on(discover_models(
            &state.backend.client,
            provider.as_object().expect("normalized provider"),
        )) {
            Ok(models) => models,
            Err(error) => return router_error_response(error),
        }
    };
    let Some(selected) = body.get("selected").filter(|value| !value.is_null()) else {
        return json_response(
            &json!({"provider":provider_id,"protocol":provider["protocol"],"available":discovered.len(),"models":discovered,"added":0}),
        );
    };
    let mut config = match state.backend.config.lock() {
        Ok(config) => config,
        Err(_) => return internal_error(),
    };
    let merged = match merge_selected_models(
        &config,
        provider_id,
        &discovered,
        selected,
        &observed_at_now(),
    ) {
        Ok(merged) => merged,
        Err(error) => return config_error(&error.to_string()),
    };
    let catalog_path = generated_catalog_path(state);
    let persisted = with_file_transaction(|transaction| -> Result<(Value, Value), ConfigError> {
        save_configuration_in_transaction(
            &merged.config,
            Some(&state.backend.config_path),
            &state.backend.vault,
            transaction,
        )?;
        let saved = load_configuration(Some(&state.backend.config_path))?;
        let catalog = server_catalog(state, &saved);
        transaction.remember(&catalog_path)?;
        write_catalog_json(&catalog_path, &catalog)?;
        Ok((saved, catalog))
    });
    let (saved, catalog) = match persisted {
        Ok(saved) => saved,
        Err(_) => return internal_error(),
    };
    *config = saved;
    json_response(&json!({
        "provider":provider_id,"protocol":provider["protocol"],"available":merged.available,
        "added":merged.added,"hidden":merged.hidden,"catalog_path":catalog_path,
        "model_count":catalog["models"].as_array().map_or(0,Vec::len),
    }))
}

pub(super) fn models_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let config = match state.backend.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => return internal_error(),
    };
    let rich = request.raw_path() == "/v1/models"
        && query_values(request.target, "client_version")
            .iter()
            .any(|value| !value.is_empty());
    if rich && preserve_native_catalog(&config).is_err() {
        return internal_error();
    }
    let catalog = server_catalog(state, &config);
    if request.raw_path().starts_with("/v1/models/") {
        let id = percent_decode(&request.raw_path()["/v1/models/".len()..], false);
        if catalog["models"].as_array().is_some_and(|models| {
            models
                .iter()
                .any(|model| model.get("slug").and_then(Value::as_str) == Some(&id))
        }) {
            return json_response(&json!({"id":id,"object":"model","created":0}));
        }
        return json_error_response(
            404,
            status_text(404),
            &format!("unknown model: {id}"),
            None,
            &[],
        );
    }
    if rich {
        let etag = emp_state::catalog_etag(&catalog);
        return match etag {
            Ok(etag) => response(
                "HTTP/1.1 200 OK",
                "application/json",
                &serde_json::to_vec(&catalog).expect("catalog JSON"),
                &[("ETag", etag.as_str())],
            ),
            Err(_) => internal_error(),
        };
    }
    let models = catalog["models"].as_array().expect("catalog models").iter()
        .filter(|model|model.get("visibility").and_then(Value::as_str).unwrap_or("list")=="list")
        .map(|model|json!({"id":model.get("slug"),"object":"model","created":0,"owned_by":"easy-multi-provider"})).collect::<Vec<_>>();
    json_response(&json!({"object":"list","data":models}))
}

fn generated_catalog_path(state: &ServerState) -> PathBuf {
    let home = state
        .backend
        .native_auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    emp_state::generated_catalog_path(Some(home))
}

fn server_catalog(state: &ServerState, config: &Value) -> Value {
    let native = load_native_catalog(config);
    let mut account_catalogs = BTreeMap::new();
    let mut duplicates = BTreeMap::new();
    let mut seen = Vec::<(String, Value)>::new();
    if let Some(auth) = native_auth_document(&state.backend.native_auth_path)
        .filter(|auth| validate_auth_json(auth).is_ok())
    {
        seen.push(("当前 Codex 登录".to_owned(), auth));
    }
    for account in config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(account_map) = account.as_object() else {
            continue;
        };
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if let Some(auth) = account
            .get("auth_file")
            .and_then(Value::as_str)
            .and_then(|path| {
                state
                    .backend
                    .vault
                    .read_encrypted_json(Path::new(path))
                    .ok()
            })
            .filter(|auth| validate_auth_json(auth).is_ok())
        {
            if let Some((source, _)) = seen
                .iter()
                .find(|(_, previous)| same_account_auth(&auth, previous))
            {
                duplicates.insert(id.to_owned(), source.clone());
            } else {
                seen.push((id.to_owned(), auth));
            }
        }
        let catalog = account_catalog(
            config.as_object().expect("config object"),
            account_map,
            &mut |account| account_catalog_headers(account, &state.backend.vault),
        );
        account_catalogs.insert(id.to_owned(), catalog);
    }
    build_catalog(config, &native, &account_catalogs, &duplicates)
}
fn json_response(value: &Value) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(value).expect("JSON response"),
        &[],
    )
}
fn config_error(message: &str) -> Vec<u8> {
    json_error_response(400, status_text(400), message, None, &[])
}
fn internal_error() -> Vec<u8> {
    json_error_response(500, status_text(500), "internal server error", None, &[])
}
