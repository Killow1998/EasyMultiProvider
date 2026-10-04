//! Api catalog.

use crate::api::failure_response::router_error_response;
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
use crate::services::catalog::refresh_catalog;
use crate::services::catalog::server_catalog;
use emp_codex::preserve_native_catalog;
use emp_router::discovery::discover_models;
use emp_state::provider_api_key;
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
    let session = request.session_token();
    if !state.sessions.contains(session.as_deref(), now) {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if request.raw_path() == "/api/config" {
        return update_configuration(request, state, &body);
    }
    if request.raw_path() == "/api/catalog/context-preference" {
        return update_catalog_context_preference(state, &body);
    }
    if request.raw_path() == "/api/catalog/refresh" {
        let (path, model_count) = match refresh_catalog(state) {
            Ok(result) => result,
            Err(()) => return internal_error(),
        };
        crate::services::account_catalog::request_refresh(state, false);
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
            let config = match state.backend.configuration.read() {
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
        let config = match state.backend.configuration.read() {
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
    match crate::services::catalog::select_models(state, provider_id, &discovered, selected) {
        Ok(selected) => json_response(&json!({
            "provider":provider_id,"protocol":provider["protocol"],"available":selected.available,
            "added":selected.added,"hidden":selected.hidden,"catalog_path":selected.catalog_path,
            "model_count":selected.model_count,
        })),
        Err(error) => change_error(error),
    }
}

fn update_configuration(request: Request<'_>, state: &ServerState, incoming: &Value) -> Vec<u8> {
    match crate::services::configuration::settings::update(state, incoming) {
        Ok(()) => read_management_request(request, state),
        Err(error) => change_error(error),
    }
}

fn update_catalog_context_preference(state: &ServerState, incoming: &Value) -> Vec<u8> {
    match crate::services::configuration::settings::context_preference(state, incoming) {
        Ok(show_context) => json_response(&json!({"catalog_show_context":show_context})),
        Err(error) => change_error(error),
    }
}

fn change_error(error: crate::services::configuration::ChangeError) -> Vec<u8> {
    match error {
        crate::services::configuration::ChangeError::Invalid(message) => config_error(&message),
        crate::services::configuration::ChangeError::Unavailable => internal_error(),
    }
}

pub(crate) fn models_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => return internal_error(),
    };
    if request.raw_path() == "/v1/models" {
        crate::services::account_catalog::request_refresh(state, false);
    }
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
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => return internal_error(),
    };
    if request.raw_path() == "/api/config" {
        return match crate::services::catalog::public_configuration(state, &config) {
            Ok(public) => json_response(&public),
            Err(_) => internal_error(),
        };
    }
    let id = percent_decode(
        request
            .raw_path()
            .strip_prefix("/api/accounts/")
            .and_then(|path| path.strip_suffix("/models"))
            .unwrap_or_default(),
        false,
    );
    match crate::services::catalog::subscription_options(state, &config, &id) {
        Ok(models) => json_response(&json!({"models":models})),
        Err(message) => config_error(message),
    }
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
