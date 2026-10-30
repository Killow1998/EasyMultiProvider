//! Freeze request, configuration and route before history replay or upstream I/O.

use crate::app::ServerState;
use crate::services::auto_review::resolve_auto_review_route;
use crate::services::providers::hydrate_provider_keys;
use emp_core::{Dialect, OpaqueJson, Protocol, ResolvedRoute, RouteResolutionError, resolve_route};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SummaryPolicy {
    Auto,
    Show,
    Hide,
}

#[derive(Debug)]
pub(crate) enum RequestPreparationError {
    ModelRequired,
    ConfigurationUnavailable,
    Route(RouteResolutionError),
    ModelSnapshotTooLarge,
    MemoryUnavailable(&'static str),
}

#[derive(Clone, Copy)]
pub(crate) enum RequestOperation {
    Responses,
    Compact,
    Images,
}

pub(crate) struct PreparedRequest {
    pub(crate) config: Value,
    pub(crate) route: ResolvedRoute,
    pub(crate) body: Value,
}

pub(crate) fn prepare_request(
    state: &ServerState,
    body: Value,
    operation: RequestOperation,
) -> Result<PreparedRequest, RequestPreparationError> {
    let config = state
        .backend
        .configuration
        .snapshot()
        .map_err(|_| RequestPreparationError::ConfigurationUnavailable)?;
    prepare_with_config(state, config, body, operation)
}

/// A policy that already selected an account must retain that configuration snapshot.
pub(super) fn prepare_with_config(
    state: &ServerState,
    mut config: Value,
    mut body: Value,
    operation: RequestOperation,
) -> Result<PreparedRequest, RequestPreparationError> {
    let request = body
        .as_object_mut()
        .ok_or(RequestPreparationError::ModelRequired)?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(RequestPreparationError::ModelRequired)?;
    hydrate_provider_keys(&mut config, &state.backend.configuration.vault);
    if let Some(config) = config.as_object_mut() {
        if emp_integration::local_auth::mode(&state.backend.accounts.native_auth_path) == "api_key"
        {
            let endpoint = config
                .get("codex_base_url")
                .and_then(Value::as_str)
                .filter(|url| url.trim_end_matches('/') != "https://chatgpt.com/backend-api/codex")
                .unwrap_or("https://api.openai.com/v1")
                .to_owned();
            config.insert("_native_base_url".to_owned(), Value::String(endpoint));
        }
        config.insert(
            "_native_auth_path".to_owned(),
            Value::String(
                state
                    .backend
                    .accounts
                    .native_auth_path
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    let automatic = match operation {
        RequestOperation::Responses => resolve_auto_review_route(state, &mut config, model),
        RequestOperation::Compact | RequestOperation::Images => None,
    };
    let mut route = automatic
        .unwrap_or_else(|| {
            resolve_route(&config, model, |config, slug, account| {
                crate::services::catalog::subscription_model(state, config, slug, account)
            })
        })
        .map_err(RequestPreparationError::Route)?;
    prepare_external_request(&config, &mut route, request)?;
    Ok(PreparedRequest {
        config,
        route,
        body,
    })
}

fn prepare_external_request(
    config: &Value,
    route: &mut ResolvedRoute,
    body: &mut Map<String, Value>,
) -> Result<(), RequestPreparationError> {
    if route.dialect == Dialect::CodexNative {
        return Ok(());
    }

    let model = route.model.value();
    if let Some(limit) = model
        .get("output_limit")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
    {
        match body.get("max_output_tokens") {
            None | Some(Value::Null) => {
                body.insert("max_output_tokens".into(), limit.into());
            }
            Some(value) if value.as_u64().is_some_and(|value| value > limit) => {
                body.insert("max_output_tokens".into(), limit.into());
            }
            _ => {}
        }
    }
    let policy = summary_policy(config, route, model);
    let supported = model
        .get("supports_reasoning_summaries")
        .and_then(Value::as_bool)
        == Some(true);
    let responses_capable = route.protocol == Protocol::Responses
        || (route.protocol == Protocol::Auto
            && emp_router::observed_protocol(route) == Some(Protocol::Responses));
    let preserve_summary = supported && responses_capable && policy != SummaryPolicy::Hide;

    let mut request_model = model.clone();
    request_model.insert(
        "_emp_reasoning_summary_policy".to_owned(),
        Value::String(
            match policy {
                SummaryPolicy::Auto => "auto",
                SummaryPolicy::Show => "show",
                SummaryPolicy::Hide => "hide",
            }
            .to_owned(),
        ),
    );
    request_model.insert(
        "_emp_preserve_reasoning_summary".to_owned(),
        Value::Bool(preserve_summary),
    );
    request_model.insert(
        "_emp_preserve_reasoning_state".to_owned(),
        Value::Bool(false),
    );
    route.model = OpaqueJson::new(request_model)
        .map_err(|_| RequestPreparationError::ModelSnapshotTooLarge)?;

    let mut reasoning = match body.remove("reasoning") {
        Some(Value::Object(reasoning)) => reasoning,
        _ => Map::new(),
    };
    if !responses_capable || !supported || policy == SummaryPolicy::Hide {
        reasoning.remove("summary");
    } else if policy == SummaryPolicy::Show {
        reasoning.insert("summary".to_owned(), Value::String("auto".to_owned()));
    }
    if !reasoning.is_empty() {
        body.insert("reasoning".to_owned(), Value::Object(reasoning));
    }
    Ok(())
}

fn summary_policy(
    config: &Value,
    route: &ResolvedRoute,
    model: &Map<String, Value>,
) -> SummaryPolicy {
    let explicit_family = ["family_id", "upstream_id"].iter().any(|field| {
        model
            .get(*field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    });
    let family = model
        .get("_emp_family")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| nonempty_model_field(model, "family_id"))
        .or_else(|| nonempty_model_field(model, "upstream_id"))
        .unwrap_or(&route.requested_model);

    let family_presentation = explicit_family
        .then(|| {
            config
                .get("catalog_family_presentations")
                .and_then(Value::as_object)
                .and_then(|presentations| presentations.get(family))
                .and_then(Value::as_object)
        })
        .flatten();
    let presentation = family_presentation.or_else(|| {
        config
            .get("catalog_presentations")
            .and_then(Value::as_object)
            .and_then(|presentations| presentations.get(&route.requested_model))
            .and_then(Value::as_object)
    });
    match presentation
        .and_then(|value| value.get("reasoning_summary"))
        .and_then(Value::as_str)
    {
        Some("show") => SummaryPolicy::Show,
        Some("hide") => SummaryPolicy::Hide,
        _ => SummaryPolicy::Auto,
    }
}

fn nonempty_model_field<'a>(model: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    model
        .get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests;
