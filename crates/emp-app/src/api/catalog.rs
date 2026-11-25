use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::request::percent_decode;
use crate::http::request::query_values;
use crate::http::response::json_error_body;
use crate::server::http::error::{EntryError, JsonReply};
use crate::services::catalog::{self, DiscoveryError, server_catalog};
use emp_codex::preserve_native_catalog;
use serde_json::Value;
use serde_json::json;
pub(crate) fn discovery_reply(
    result: Result<Value, DiscoveryError>,
    upstream_error: impl FnOnce(emp_router::RouterError) -> JsonReply,
) -> JsonReply {
    match result {
        Ok(value) => JsonReply::ok_json(&value),
        Err(DiscoveryError::Invalid(message)) => JsonReply::error(400, &message, None),
        Err(DiscoveryError::Unavailable) => EntryError::internal().reply(),
        Err(DiscoveryError::LocalCli(code)) => {
            let (status, code, message) = crate::services::claude_cli::failure_details(code);
            JsonReply::error(status, message, Some(code))
        }
        Err(DiscoveryError::Upstream(error)) => upstream_error(error),
    }
}

pub(crate) fn metadata_request(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> JsonReply {
    discovery_reply(catalog::model_metadata(request_id, state, body), |error| {
        if error.status() == 500 && error.to_string() == "internal server error" {
            EntryError::internal().reply()
        } else {
            metadata_error_response(error)
        }
    })
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
        crate::services::catalog::request_refresh(state, false);
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
    let mut catalog = server_catalog(state, &config);
    catalog::project_catalog(request.hooks, &mut catalog["models"], "slug");
    if let Some(raw_id) = request.raw_path().strip_prefix("/v1/models/") {
        let id = percent_decode(raw_id, false);
        let canonical =
            emp_state::config::resolve_client_model(&config, &id).unwrap_or_else(|| id.clone());
        if catalog["models"].as_array().is_some_and(|models| {
            models
                .iter()
                .any(|model| model.get("slug").and_then(Value::as_str) == Some(canonical.as_str()))
        }) {
            return Ok(JsonReply::ok_json(
                &json!({"id":id,"object":"model","created":0}),
            ));
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
        return Ok(JsonReply::ok_json(&catalog)
            .with_header("ETag", etag.map_err(|_| EntryError::internal())?));
    }
    let models = catalog["models"].as_array().expect("catalog models").iter()
        .filter(|model|model.get("visibility").and_then(Value::as_str).unwrap_or("list")=="list")
        .map(|model|json!({"id":model.get("slug"),"object":"model","created":0,"owned_by":"easy-multi-provider"})).collect::<Vec<_>>();
    Ok(JsonReply::ok_json(&json!({"object":"list","data":models})))
}

pub(crate) fn read_configuration(
    state: &ServerState,
    hooks: Option<&crate::modules::hooks::HookOperation>,
) -> JsonReply {
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => return EntryError::internal().reply(),
    };
    let mut public = crate::services::catalog::public_configuration(state, &config, hooks);
    public["provider_balances"] = crate::services::provider_balance::snapshot(state, &config);
    public["provider_quotas"] = crate::services::cpa_quota::snapshot(state, &config);
    public["app_icon"] = state.backend.app_icon.snapshot().metadata();
    JsonReply::ok_json(&public)
}

pub(crate) fn provider_models_request(id: Option<&str>, state: &ServerState) -> JsonReply {
    let result = match id {
        Some(id) => catalog::saved_provider_models(state, &percent_decode(id, false)),
        None if state.backend.configuration.read().is_err() => Err(DiscoveryError::Unavailable),
        None => Err(DiscoveryError::Invalid(
            "Subscription account is unavailable".into(),
        )),
    };
    discovery_reply(result, |_| EntryError::internal().reply())
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
