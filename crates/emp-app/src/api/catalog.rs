//! Api catalog.
use emp_codex::management_views::subscription_model_options;

use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::percent_decode;
use crate::http::request::query_values;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use crate::services::accounts::account_catalog_headers;
use crate::services::accounts::native_account_snapshot;
use crate::services::accounts::regular_file;
use crate::services::catalog::catalog_sources;
use crate::services::catalog::generated_catalog_path;
use crate::services::catalog::refresh_catalog;
use crate::services::catalog::server_catalog;
use crate::services::failures::router_error_response;
use emp_codex::account_catalog;
use emp_codex::load_native_catalog;
use emp_codex::management_views::model_views;
use emp_codex::preserve_native_catalog;
use emp_codex::subscription_contexts::validate_subscription_contexts;
use emp_router::discovery::discover_models;
use emp_state::ConfigError;
use emp_state::canonicalize_private_paths;
use emp_state::discovery_merge::merge_selected_models;
use emp_state::filesystem::write_catalog_json;
use emp_state::load_configuration;
use emp_state::merge_web_update;
use emp_state::migrate_duplicate_native_visibility;
use emp_state::observed_at_now;
use emp_state::provider_api_key;
use emp_state::public_configuration_with_file_status;
use emp_state::save_configuration_in_transaction;
use emp_state::with_file_transaction;
use serde_json::Value;
use serde_json::json;
use std::net::TcpStream;

pub(crate) fn management_request(
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
        let (path, model_count) = match refresh_catalog(state) {
            Ok(result) => result,
            Err(()) => return internal_error(),
        };
        return json_response(
            &json!({"status":"ok","catalog_path":path,"model_count":model_count}),
        );
    }
    if request.raw_path() == "/api/models/metadata" {
        let Some(provider_id) = body.get("provider").and_then(Value::as_str) else {
            return config_error("provider and model are required");
        };
        let Some(mut model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
            return config_error("provider and model are required");
        };
        let mut provider = {
            let config = match state.backend.configuration.config.lock() {
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
        if let Some(upstream) = model.strip_prefix(&format!("{provider_id}/")) {
            model = upstream.to_owned();
        }
        provider["api_key"] = json!(provider_api_key(
            &provider,
            &state.backend.configuration.vault
        ));
        let Some(provider) = provider.as_object() else {
            return internal_error();
        };
        let result =
            state
                .backend
                .transport
                .runtime
                .block_on(emp_router::discovery::model_metadata(
                    &state.backend.transport.client,
                    provider,
                    &model,
                ));
        return match result {
            Ok(value) => json_response(&value),
            Err(error) if error.status() == 500 && error.to_string() == "internal server error" => {
                internal_error()
            }
            Err(error) => metadata_error_response(error),
        };
    }
    let Some(provider_id) = body
        .get("provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return config_error("provider is required");
    };
    let mut provider = {
        let config = match state.backend.configuration.config.lock() {
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
    provider["api_key"] = json!(provider_api_key(
        &provider,
        &state.backend.configuration.vault
    ));
    let discovered = {
        let _guard = match state.backend.configuration.discovery_lock.lock() {
            Ok(guard) => guard,
            Err(_) => return internal_error(),
        };
        match state.backend.transport.runtime.block_on(discover_models(
            &state.backend.transport.client,
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
    let mut config = match state.backend.configuration.config.lock() {
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
            Some(&state.backend.configuration.config_path),
            &state.backend.configuration.vault,
            transaction,
        )?;
        let saved = load_configuration(Some(&state.backend.configuration.config_path))?;
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
    let mut current = match state.backend.configuration.config.lock() {
        Ok(config) => config,
        Err(_) => return internal_error(),
    };
    let mut updated = match merge_web_update(&current, incoming) {
        Ok(updated) => updated,
        Err(error) => return config_error(&error.to_string()),
    };
    if let Err(error) =
        canonicalize_private_paths(&mut updated, &state.backend.configuration.config_path)
    {
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
            Some(&state.backend.configuration.config_path),
            &state.backend.configuration.vault,
            transaction,
        )?;
        load_configuration(Some(&state.backend.configuration.config_path))
    });
    let saved = match saved {
        Ok(saved) => saved,
        Err(_) => return internal_error(),
    };
    *current = saved;
    drop(current);
    crate::services::runtime::mark_active_pending(state, "EMP configuration changed");
    read_management_request(request, state)
}

pub(crate) fn models_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let config = match state.backend.configuration.config.lock() {
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
pub(crate) fn read_management_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let config = match state.backend.configuration.config.lock() {
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
        public["emp_version"] = json!(crate::VERSION);
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
            &mut |account| account_catalog_headers(account, &state.backend.configuration.vault),
        )
    };
    json_response(&json!({"models":subscription_model_options(&catalog)}))
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

fn metadata_error_response(error: emp_router::RouterError) -> Vec<u8> {
    let error_class = error.error_class().as_str();
    let code = if error.error_class() == emp_transport::FailureClass::RateLimit {
        "rate_limit_exceeded"
    } else {
        error.failure_reason().unwrap_or(error_class)
    };
    let mut detail = json!({
        "code":code,
        "type":error_class,
        "message":error.to_string(),
    });
    if let Some(reason) = error.failure_reason() {
        detail["failure_reason"] = Value::String(reason.to_owned());
    }
    if let Some(delay) = error.retry_after_seconds() {
        detail["retry_after_seconds"] = Value::from(delay);
    }
    let body = serde_json::to_vec(&json!({"error":detail})).expect("JSON error response");
    let retry = error.retry_after_seconds().map(|delay| delay.to_string());
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    response(
        &format!(
            "HTTP/1.1 {} {}",
            error.status(),
            status_text(error.status())
        ),
        "application/json",
        &body,
        &headers,
    )
}
pub(crate) fn internal_error() -> Vec<u8> {
    json_error_response(500, status_text(500), "internal server error", None, &[])
}
