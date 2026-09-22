//! Authenticated model discovery, selected-model persistence, and Codex catalogs.
use super::{
    Request, ServerState, account_catalog_headers, body_error_response, cross_origin_response,
    json_error_response, native_account_snapshot, native_auth_document, percent_decode,
    query_values, read_json_body, regular_file, response, router_error_response, same_origin,
    status_text, unauthorized_response,
};
use emp_codex::{
    account_catalog, load_native_catalog,
    management_views::{model_views, subscription_model_options},
    merged_catalog::build_catalog,
    preserve_native_catalog,
    subscription_contexts::validate_subscription_contexts,
};
use emp_router::discovery::discover_models;
use emp_state::{
    ConfigError, VaultStore, canonicalize_private_paths, discovery_merge::merge_selected_models,
    duplicate_account_status, filesystem::write_catalog_json, load_configuration, merge_web_update,
    migrate_duplicate_native_visibility, observed_at_now, provider_api_key,
    public_configuration_with_file_status, save_configuration_in_transaction,
    with_file_transaction,
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
    if request.raw_path() == "/api/config" {
        return update_configuration(request, state, &body);
    }
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

fn update_configuration(request: Request<'_>, state: &ServerState, incoming: &Value) -> Vec<u8> {
    let mut current = match state.backend.config.lock() {
        Ok(config) => config,
        Err(_) => return internal_error(),
    };
    let mut updated = match merge_web_update(&current, incoming) {
        Ok(updated) => updated,
        Err(error) => return config_error(&error.to_string()),
    };
    if let Err(error) = canonicalize_private_paths(&mut updated, &state.backend.config_path) {
        return config_error(&error.to_string());
    }
    let sources = catalog_sources(state, &updated);
    if let Err(error) =
        validate_subscription_contexts(&updated, Some(&current), &sources.native, &sources.accounts)
    {
        return config_error(&error.to_string());
    }
    let (updated, _) = migrate_duplicate_native_visibility(&updated, &sources.duplicates);
    let saved = with_file_transaction(|transaction| -> Result<Value, ConfigError> {
        save_configuration_in_transaction(
            &updated,
            Some(&state.backend.config_path),
            &state.backend.vault,
            transaction,
        )?;
        load_configuration(Some(&state.backend.config_path))
    });
    let saved = match saved {
        Ok(saved) => saved,
        Err(_) => return internal_error(),
    };
    *current = saved;
    drop(current);
    read_management_request(request, state)
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

/// Caller has already checked the browser session and same-origin boundary.
pub(super) fn read_management_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let config = match state.backend.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => return internal_error(),
    };
    if request.raw_path() == "/api/config" {
        let sources = catalog_sources(state, &config);
        let mut public =
            match public_configuration_with_file_status(&config, &sources.duplicates, regular_file)
            {
                Ok(public) => public,
                Err(_) => return internal_error(),
            };
        public["emp_version"] = json!(super::VERSION);
        public["native_account"] = native_account_snapshot(state, &config);
        let views = model_views(
            &config,
            &sources.native,
            &sources.accounts,
            &sources.duplicates,
        );
        public
            .as_object_mut()
            .expect("public config")
            .extend(views.as_object().expect("model views").clone());
        return json_response(&public);
    }
    let id = percent_decode(
        request
            .raw_path()
            .strip_prefix("/api/accounts/")
            .and_then(|path| path.strip_suffix("/models"))
            .unwrap_or_default(),
        false,
    );
    let catalog = if id == "@native" {
        load_native_catalog(&config)
    } else {
        let Some(account) = config["accounts"]
            .as_array()
            .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
            .filter(|account| {
                account["auth_file"]
                    .as_str()
                    .is_some_and(|path| !path.is_empty())
            })
        else {
            return config_error("Subscription account is unavailable");
        };
        account_catalog(
            config.as_object().expect("config"),
            account.as_object().expect("account"),
            &mut |account| account_catalog_headers(account, &state.backend.vault),
        )
    };
    json_response(&json!({"models":subscription_model_options(&catalog)}))
}

fn generated_catalog_path(state: &ServerState) -> PathBuf {
    let home = state
        .backend
        .native_auth_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    emp_state::generated_catalog_path(Some(home))
}

struct CatalogSources {
    native: Value,
    accounts: BTreeMap<String, Value>,
    duplicates: BTreeMap<String, String>,
}

fn server_catalog(state: &ServerState, config: &Value) -> Value {
    let sources = catalog_sources(state, config);
    build_catalog(
        config,
        &sources.native,
        &sources.accounts,
        &sources.duplicates,
    )
}

fn catalog_sources(state: &ServerState, config: &Value) -> CatalogSources {
    let native = load_native_catalog(config);
    let mut account_catalogs = BTreeMap::new();
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
        let catalog = account_catalog(
            config.as_object().expect("config object"),
            account_map,
            &mut |account| account_catalog_headers(account, &state.backend.vault),
        );
        account_catalogs.insert(id.to_owned(), catalog);
    }
    CatalogSources {
        native,
        accounts: account_catalogs,
        duplicates: duplicate_accounts(
            config,
            &state.backend.vault,
            &state.backend.native_auth_path,
        ),
    }
}

pub(super) fn duplicate_accounts(
    config: &Value,
    vault: &VaultStore,
    native_auth_path: &Path,
) -> BTreeMap<String, String> {
    let native = native_auth_document(native_auth_path);
    let credentials = config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|account| {
            let id = account.get("id")?.as_str()?;
            let path = account.get("auth_file")?.as_str()?;
            if path.is_empty() {
                return None;
            }
            vault
                .read_encrypted_json(Path::new(path))
                .ok()
                .map(|auth| (id.to_owned(), auth))
        })
        .collect::<Vec<_>>();
    duplicate_account_status(native.as_ref(), &credentials)
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
