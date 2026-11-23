use crate::api::failure_response::router_error_response;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::request::percent_decode;
use crate::http::request::query_values;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::json_error_body;
use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::server::http::error::{EntryError, JsonReply};
use crate::server::http::management::ManagementSession;
use crate::services::catalog::discovery::{self, DiscoveryError};
use crate::services::catalog::refresh_for_management;
use crate::services::catalog::server_catalog;
use emp_codex::preserve_native_catalog;
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
    if let Err(error) = ManagementSession::require(request, state, now) {
        return error.wire_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if request.raw_path() == "/api/language" {
        let Some(language) = body["language"].as_str() else {
            return config_error("language is required");
        };
        return match crate::services::configuration::update_language(state, language) {
            Ok(()) => json_response(&json!({"language":language})),
            Err(error) => config_error(&error),
        };
    }
    if request.raw_path() == "/api/config" {
        return match crate::services::configuration::settings::update(
            request.observation_id,
            state,
            &body,
        ) {
            Ok(()) => read_configuration(state).wire_response(),
            Err(error) => change_error(error),
        };
    }
    if request.raw_path() == "/api/catalog/context-preference" {
        return update_catalog_context_preference(request, state, &body);
    }
    if request.raw_path() == "/api/catalog/refresh" {
        let (path, model_count) = match refresh_for_management(request.observation_id, state) {
            Ok(result) => result,
            Err(()) => return internal_error(),
        };
        return json_response(
            &json!({"status":"ok","catalog_path":path,"model_count":model_count}),
        );
    }
    if request.raw_path() == "/api/models/metadata" {
        return metadata_request(request.observation_id, state, &body).wire_response();
    }
    let result = if request.raw_path() == "/api/decisions/models" {
        crate::api::decisions::discover(state, &body)
    } else {
        discovery::discover(request.observation_id, state, &body)
    };
    match discovery_reply(result) {
        Ok(reply) => reply.wire_response(),
        Err(error) => router_error_response(error),
    }
}

fn discovery_reply(
    result: Result<Value, DiscoveryError>,
) -> Result<JsonReply, emp_router::RouterError> {
    match result {
        Ok(value) => Ok(catalog_json(&value)),
        Err(DiscoveryError::Invalid(message)) => {
            Ok(JsonReply::new(400, json_error_body(&message, None)))
        }
        Err(DiscoveryError::Unavailable) => Ok(EntryError::internal().reply()),
        Err(DiscoveryError::LocalCli(code)) => {
            let (status, code, message) = crate::services::claude_cli::failure_details(code);
            Ok(JsonReply::new(status, json_error_body(message, Some(code))))
        }
        Err(DiscoveryError::Upstream(error)) => Err(error),
    }
}

pub(crate) fn metadata_request(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> JsonReply {
    discovery_reply(discovery::metadata(request_id, state, body)).unwrap_or_else(|error| {
        if error.status() == 500 && error.to_string() == "internal server error" {
            EntryError::internal().reply()
        } else {
            metadata_error_response(error)
        }
    })
}

fn update_catalog_context_preference(
    request: Request<'_>,
    state: &ServerState,
    incoming: &Value,
) -> Vec<u8> {
    match crate::services::configuration::settings::context_preference(
        request.observation_id,
        state,
        incoming,
    ) {
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

pub(crate) fn models_path(path: &str) -> bool {
    path == "/v1/models" || path.starts_with("/v1/models/")
}

pub(crate) fn models_request(
    request: Request<'_>,
    state: &ServerState,
) -> Result<JsonReply, EntryError> {
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => return Err(EntryError::internal()),
    };
    if request.raw_path() == "/v1/models" {
        crate::services::account_catalog::request_refresh(state, false);
    }
    let versions = query_values(request.target, "client_version");
    let client_version = versions
        .iter()
        .find(|version| !version.is_empty())
        .map(String::as_str);
    let rich = request.raw_path() == "/v1/models" && client_version.is_some();
    if rich && preserve_native_catalog(&config).is_err() {
        return Err(EntryError::internal());
    }
    let catalog = server_catalog(state, &config);
    if let Some(raw_id) = request.raw_path().strip_prefix("/v1/models/") {
        let id = percent_decode(raw_id, false);
        let canonical =
            emp_state::config::resolve_client_model(&config, &id).unwrap_or_else(|| id.clone());
        if catalog["models"].as_array().is_some_and(|models| {
            models
                .iter()
                .any(|model| model.get("slug").and_then(Value::as_str) == Some(canonical.as_str()))
        }) {
            return Ok(catalog_json(&json!({"id":id,"object":"model","created":0})));
        }
        return Ok(JsonReply::new(
            404,
            json_error_body(&format!("unknown model: {id}"), None),
        ));
    }
    let catalog = emp_state::config::client_catalog(&config, catalog);
    if rich {
        let etag = emp_state::catalog_etag(&catalog);
        let catalog = emp_codex::merged_catalog::for_client_version(catalog, client_version);
        return Ok(
            catalog_json(&catalog).with_header("ETag", etag.map_err(|_| EntryError::internal())?)
        );
    }
    let models = catalog["models"].as_array().expect("catalog models").iter()
        .filter(|model|model.get("visibility").and_then(Value::as_str).unwrap_or("list")=="list")
        .map(|model|json!({"id":model.get("slug"),"object":"model","created":0,"owned_by":"easy-multi-provider"})).collect::<Vec<_>>();
    Ok(catalog_json(&json!({"object":"list","data":models})))
}

pub(crate) fn read_configuration(state: &ServerState) -> JsonReply {
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => return EntryError::internal().reply(),
    };
    let mut public = crate::services::catalog::public_configuration(state, &config);
    public["provider_balances"] = crate::services::provider_balance::snapshot(state, &config);
    public["provider_quotas"] = crate::services::cpa_quota::snapshot(state, &config);
    public["app_icon"] = state.backend.app_icon.snapshot().metadata();
    catalog_json(&public)
}

pub(crate) fn provider_models_path(path: &str) -> bool {
    path.starts_with("/api/providers/") && path.ends_with("/models")
}

pub(crate) fn provider_models_request(path: &str, state: &ServerState) -> JsonReply {
    let result = match path
        .strip_prefix("/api/providers/")
        .and_then(|path| path.strip_suffix("/models"))
    {
        Some(id) => discovery::saved_models(state, &percent_decode(id, false)),
        None if state.backend.configuration.read().is_err() => Err(DiscoveryError::Unavailable),
        None => Err(DiscoveryError::Invalid(
            "Subscription account is unavailable".into(),
        )),
    };
    discovery_reply(result).unwrap_or_else(|_| EntryError::internal().reply())
}

fn catalog_json(value: &Value) -> JsonReply {
    JsonReply::new(200, serde_json::to_vec(value).expect("JSON response"))
}

fn json_response(value: &Value) -> Vec<u8> {
    catalog_json(value).wire_response()
}
fn config_error(message: &str) -> Vec<u8> {
    json_error_response(400, status_text(400), message, None, &[])
}

fn metadata_error_response(error: emp_router::RouterError) -> JsonReply {
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
    let reply = JsonReply::new(error.status(), body);
    match error.retry_after_seconds() {
        Some(delay) => reply.with_header("Retry-After", delay.to_string()),
        None => reply,
    }
}

pub(crate) fn internal_error() -> Vec<u8> {
    json_error_response(500, status_text(500), "internal server error", None, &[])
}
