//! One-time, request-local preparation shared by portable Responses endpoints.
//!
//! Keep the request body, configuration and route paired so selection, context
//! checks and forwarding observe the same decision without persisting markers.

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

impl SummaryPolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Show => "show",
            Self::Hide => "hide",
        }
    }
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

/// Freeze routing and presentation together before any history replay or I/O.
/// Compact and image tools resolve the requested account directly; auto-review
/// fallback belongs only to Responses turns. Wire errors and WebSocket
/// continuation state remain with their respective adapters.
pub(crate) fn prepare_request(
    state: &ServerState,
    body: Value,
    operation: RequestOperation,
) -> Result<PreparedRequest, RequestPreparationError> {
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| RequestPreparationError::ConfigurationUnavailable)?
        .clone();
    prepare_with_config(state, config, body, operation)
}

/// A policy that already selected an account must retain that configuration snapshot.
pub(super) fn prepare_with_config(
    state: &ServerState,
    mut config: Value,
    body: Value,
    operation: RequestOperation,
) -> Result<PreparedRequest, RequestPreparationError> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(RequestPreparationError::ModelRequired)?;
    hydrate_provider_keys(&mut config, &state.backend.configuration.vault);
    if let Some(config) = config.as_object_mut() {
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
    let route = automatic
        .unwrap_or_else(|| {
            resolve_route(&config, model, |config, slug, account| {
                crate::services::catalog::subscription_model(state, config, slug, account)
            })
        })
        .map_err(RequestPreparationError::Route)?;
    let (route, body) = prepare_external_request(&config, route, body)?;
    Ok(PreparedRequest {
        config,
        route,
        body,
    })
}

/// Apply Python's external reasoning-summary policy exactly once to an owned
/// body and its frozen route. Native routes keep both inputs untouched.
fn prepare_external_request(
    config: &Value,
    mut route: ResolvedRoute,
    mut body: Value,
) -> Result<(ResolvedRoute, Value), RequestPreparationError> {
    if route.dialect == Dialect::CodexNative {
        return Ok((route, body));
    }

    let model = route.model.value();
    let policy = summary_policy(config, &route, model);
    let supported = model
        .get("supports_reasoning_summaries")
        .and_then(Value::as_bool)
        == Some(true);
    let responses_capable = route.protocol == Protocol::Responses
        || (route.protocol == Protocol::Auto
            && emp_router::observed_protocol(&route) == Some(Protocol::Responses));
    let preserve_summary = supported && responses_capable && policy != SummaryPolicy::Hide;

    let mut request_model = model.clone();
    request_model.insert(
        "_emp_reasoning_summary_policy".to_owned(),
        Value::String(policy.as_str().to_owned()),
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

    if let Some(body) = body.as_object_mut() {
        let mut reasoning = body
            .get("reasoning")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !responses_capable || !supported || policy == SummaryPolicy::Hide {
            reasoning.remove("summary");
        } else if policy == SummaryPolicy::Show {
            reasoning.insert("summary".to_owned(), Value::String("auto".to_owned()));
        }
        if reasoning.is_empty() {
            body.remove("reasoning");
        } else {
            body.insert("reasoning".to_owned(), Value::Object(reasoning));
        }
    }
    Ok((route, body))
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
