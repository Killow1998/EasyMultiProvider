//! Freeze request, configuration and route before history replay or upstream I/O.

use crate::app::ServerState;
use crate::modules::hooks::{HookOperation, HookPhase};
use crate::services::auto_review::resolve_auto_review_route;
use crate::services::providers::hydrate_provider_keys;
use emp_domain::{
    Dialect, OpaqueJson, Protocol, ResolvedRoute, RouteResolutionError, resolve_route,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

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
    TeamAccess(super::team::RequestDenied),
    HookRoute(RouteResolutionError),
    HookAccess,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
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
    incoming: &BTreeMap<String, String>,
    hooks: Option<&HookOperation>,
) -> Result<PreparedRequest, RequestPreparationError> {
    let config = state
        .backend
        .configuration
        .snapshot()
        .map_err(|_| RequestPreparationError::ConfigurationUnavailable)?;
    prepare_with_config(state, config, body, operation, incoming, hooks)
}

/// A policy that already selected an account must retain that configuration snapshot.
pub(super) fn prepare_with_config(
    state: &ServerState,
    mut config: Value,
    mut body: Value,
    operation: RequestOperation,
    incoming: &BTreeMap<String, String>,
    hooks: Option<&HookOperation>,
) -> Result<PreparedRequest, RequestPreparationError> {
    let request = body
        .as_object_mut()
        .ok_or(RequestPreparationError::ModelRequired)?;
    let model = request
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(RequestPreparationError::ModelRequired)?;
    if let Some(canonical) = emp_state::config::resolve_client_model(&config, model) {
        request.insert("model".to_owned(), canonical.into());
    }
    let model = request["model"].as_str().expect("validated model");
    if let Some(hooks) = hooks {
        let accounts = config["accounts"]
            .as_array_mut()
            .expect("normalized accounts");
        project_accounts(hooks, accounts, model);
    }
    state
        .backend
        .team
        .project_subscription_credentials(state, &mut config);
    hydrate_provider_keys(&mut config, &state.backend.configuration.vault);
    if let Some(config) = config.as_object_mut() {
        if emp_codex::configuration::local_auth::mode(&state.backend.codex.auth_path) == "api_key" {
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
            Value::String(state.backend.codex.auth_path.to_string_lossy().into_owned()),
        );
    }
    let resolve = |config: &Value, model: &str| {
        resolve_route(config, model, |config, slug, account| {
            super::catalog::subscription_model(state, config, slug, account)
        })
    };
    let selected = if let Some(hooks) = hooks.filter(|hooks| hooks.has(HookPhase::ResolveRoute)) {
        let mut candidate = resolve(&config, model).ok();
        let ids = candidate
            .iter()
            .map(|route| route.requested_model.clone())
            .collect::<Vec<_>>();
        hooks
            .dispatch(
                HookPhase::ResolveRoute,
                || json!({"model":model,"candidate_ids":ids}),
                |_, id: String| {
                    if ids.contains(&id) {
                        Ok(candidate.take())
                    } else {
                        Err(())
                    }
                },
            )
            .map_err(|()| {
                RequestPreparationError::HookRoute(RouteResolutionError::ProviderUnavailable(
                    model.into(),
                ))
            })?
    } else {
        None
    };
    let automatic = selected.map(Ok).or_else(|| {
        super::aggregate_routing::resolve(state, &config, request, incoming).or_else(|| {
            match operation {
                RequestOperation::Responses => resolve_auto_review_route(state, &mut config, model),
                RequestOperation::Compact | RequestOperation::Images => None,
            }
        })
    });
    let mut route = automatic
        .unwrap_or_else(|| resolve(&config, model))
        .map_err(RequestPreparationError::Route)?;
    state
        .backend
        .team
        .check_model_route(
            state,
            &route,
            request.get("service_tier").and_then(Value::as_str),
        )
        .map_err(RequestPreparationError::TeamAccess)?;
    if !matches!(operation, RequestOperation::Images) {
        check_hook_access(
            hooks,
            &route,
            request.get("service_tier").and_then(Value::as_str),
            operation,
        )
        .map_err(|()| RequestPreparationError::HookAccess)?;
        state
            .backend
            .team
            .check_route_budget(state, &route)
            .map_err(RequestPreparationError::TeamAccess)?;
    }
    prepare_external_request(&config, &mut route, request)?;
    Ok(PreparedRequest {
        config,
        route,
        body,
    })
}

fn project_accounts(hooks: &HookOperation, accounts: &mut Vec<Value>, model: &str) {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct AccountRef {
        id: String,
        source_id: String,
    }
    if !hooks.has(HookPhase::ProjectAccounts) {
        return;
    }
    let base = accounts.len();
    let facts: Vec<_> = accounts
        .iter()
        .map(|account| json!({"id":account["id"], "prefix":account["prefix"]}))
        .collect();
    let _ = hooks.dispatch::<Vec<AccountRef>, ()>(
        HookPhase::ProjectAccounts,
        || json!({"model":model, "accounts":facts}),
        |_, refs| {
            let mut added: Vec<Value> = Vec::new();
            for account in refs {
                if account.id.is_empty()
                    || !account
                        .id
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                    || accounts
                        .iter()
                        .chain(&added)
                        .any(|item| item["id"] == account.id || item["prefix"] == account.id)
                {
                    return Err(());
                }
                let mut source = accounts[..base]
                    .iter()
                    .find(|item| item["id"] == account.source_id)
                    .cloned()
                    .ok_or(())?;
                source["prefix"] = account.id.into();
                added.push(source);
            }
            accounts.extend(added);
            Ok(None)
        },
    );
}

pub(crate) fn check_hook_access(
    hooks: Option<&HookOperation>,
    route: &ResolvedRoute,
    tier: Option<&str>,
    operation: RequestOperation,
) -> Result<(), ()> {
    if let Some(hooks) = hooks {
        if hooks.cancelled() {
            return Err(());
        }
        if hooks.dispatch(
            HookPhase::CheckAccess,
            || json!({
                "provider_id":route.provider_id, "model":route.requested_model, "service_tier":tier,
                "account_id":route.provider.value().get("account").and_then(|account| account.get("id")),
                "operation":operation,
            }),
            |_, allowed: bool| Ok((!allowed).then_some(())),
        )?.is_some() || hooks.cancelled() {
            return Err(());
        }
    }
    Ok(())
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
    let configured_limit = model
        .get("output_limit")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0);
    let requested = body.get("max_output_tokens");
    let implicit = requested.is_none_or(Value::is_null);
    let source = model
        .get("capability_sources")
        .and_then(|sources| sources["output_limit"]["source"].as_str());
    let user_limit = !matches!(
        source,
        Some("catalog" | "official" | "advertised" | "observed")
    );
    if let Some(limit) = configured_limit
        && ((implicit && user_limit)
            || requested.and_then(Value::as_u64).is_some_and(|n| n > limit))
    {
        body.insert("max_output_tokens".into(), limit.into());
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
    request_model.insert("_emp_flexible_output_limit".into(), Value::Bool(true));
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
    if let Some(input) = body.get_mut("input")
        && let Some(effort) = take_effort_updates(input)
    {
        reasoning.insert("effort".into(), effort);
    }
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

fn take_effort_updates(input: &mut Value) -> Option<Value> {
    let mut latest = None;
    let mut keep = |item: &mut Value| {
        if item["type"] == "configuration_update"
            && let Some(effort) = item.pointer_mut("/reasoning/effort")
            && (effort.is_string() || effort.is_number())
        {
            latest = Some(effort.take());
            return false;
        }
        true
    };
    if let Some(items) = input.as_array_mut() {
        items.retain_mut(keep);
    } else if input.is_object() && !keep(input) {
        *input = Value::Array(Vec::new());
    }
    latest
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
