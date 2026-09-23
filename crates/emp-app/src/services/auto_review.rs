use crate::app::ServerState;
use crate::services::accounts::{account_catalog_headers, regular_file};
use emp_codex::subscription_route_model;
use emp_core::{ResolvedRoute, RouteResolutionError, RouteSource, resolved_route_from_parts};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::time::Instant;

const AUTO_REVIEW_MODEL_ID: &str = "codex-auto-review";
const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

#[derive(Clone)]
struct Candidate {
    id: String,
    native: bool,
    headroom: Option<f64>,
    order: usize,
    cooling: bool,
}

pub(crate) fn is_auto_review_model(model: &str) -> bool {
    model == AUTO_REVIEW_MODEL_ID || model.ends_with("/codex-auto-review")
}

pub(crate) fn resolve_auto_review_route(
    state: &ServerState,
    config: &mut Value,
    requested_model: &str,
) -> Option<Result<ResolvedRoute, RouteResolutionError>> {
    if !is_auto_review_model(requested_model) {
        return None;
    }
    let config_object = config.as_object()?;
    let native_available = regular_file(&state.backend.accounts.native_auth_path);
    let native_quota = state
        .backend
        .accounts
        .native_quota
        .lock()
        .ok()
        .and_then(|quota| quota.clone());
    let now = Instant::now();
    let cooling = state
        .auto_review_cooldowns
        .lock()
        .ok()
        .map(|cooldowns| {
            cooldowns
                .iter()
                .filter(|(_, until)| **until > now)
                .map(|(account_id, _)| account_id.clone())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let candidates = automatic_review_candidates(
        config_object,
        native_quota.as_ref(),
        native_available,
        &cooling,
    );
    config
        .as_object_mut()?
        .insert("_auto_review_candidates".to_owned(), json!(candidates));
    route_from_candidates(state, config, requested_model)
}

pub(crate) fn automatic_review_candidates(
    config: &Map<String, Value>,
    native_quota: Option<&Value>,
    native_available: bool,
    cooling: &BTreeSet<String>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if native_available {
        candidates.push(Candidate {
            id: "@native".to_owned(),
            native: true,
            headroom: quota_headroom(native_quota),
            order: 0,
            cooling: cooling.contains("@native"),
        });
    }
    if let Some(accounts) = config.get("accounts").and_then(Value::as_array) {
        for (order, account) in accounts.iter().enumerate() {
            let Some(account) = account.as_object() else {
                continue;
            };
            let Some(id) = account.get("id").and_then(Value::as_str) else {
                continue;
            };
            if id.is_empty()
                || account.get("enabled") == Some(&Value::Bool(false))
                || !account.get("auth_file").is_some_and(is_python_truthy)
                || account.get("credential_status").and_then(Value::as_str) == Some("invalid")
            {
                continue;
            }
            candidates.push(Candidate {
                id: id.to_owned(),
                native: false,
                headroom: quota_headroom(account.get("quota")),
                order,
                cooling: cooling.contains(id),
            });
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut healthy = candidates
        .iter()
        .filter(|candidate| {
            !candidate.cooling && candidate.headroom.is_none_or(|headroom| headroom > 0.0)
        })
        .cloned()
        .collect::<Vec<_>>();
    if healthy.is_empty() {
        healthy = candidates
            .iter()
            .filter(|candidate| candidate.headroom.is_none_or(|headroom| headroom > 0.0))
            .cloned()
            .collect();
    }
    if healthy.is_empty() {
        healthy = candidates;
    }

    let mut native = healthy
        .iter()
        .filter(|candidate| candidate.native)
        .cloned()
        .collect::<Vec<_>>();
    let mut imported = healthy
        .into_iter()
        .filter(|candidate| !candidate.native)
        .collect::<Vec<_>>();
    imported.sort_by(|left, right| {
        match (left.headroom, right.headroom) {
            (Some(left), Some(right)) => right.total_cmp(&left),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.order.cmp(&right.order))
    });
    native.append(&mut imported);
    native.into_iter().map(|candidate| candidate.id).collect()
}

pub(crate) fn quota_headroom(quota: Option<&Value>) -> Option<f64> {
    let quota = quota?.as_object()?;
    if quota
        .get("credits")
        .and_then(Value::as_object)
        .and_then(|credits| credits.get("spend_control_reached"))
        == Some(&Value::Bool(true))
    {
        return Some(0.0);
    }
    if let Some(remaining) = quota
        .get("credits")
        .and_then(Value::as_object)
        .and_then(|credits| credits.get("individual_limit"))
        .and_then(Value::as_object)
        .and_then(|individual| individual.get("remaining_percent"))
        .and_then(finite_number)
        && remaining <= 0.0
    {
        return Some(0.0);
    }

    let limits = quota.get("rate_limits")?.as_object()?;
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|name| limits.get(name).and_then(Value::as_object))
        .filter_map(|window| {
            let used = window
                .get("usedPercent")
                .or_else(|| window.get("used_percent"))
                .and_then(finite_number)?;
            Some((100.0 - used).clamp(0.0, 100.0))
        })
        .reduce(f64::min)
}

fn route_from_candidates(
    state: &ServerState,
    config: &Value,
    requested_model: &str,
) -> Option<Result<ResolvedRoute, RouteResolutionError>> {
    if !is_auto_review_model(requested_model) {
        return None;
    }
    let config = config.as_object()?;
    let order = config
        .get("_auto_review_candidates")
        .and_then(Value::as_array)?;
    let accounts = config.get("accounts").and_then(Value::as_array);
    for account_id in order.iter().filter_map(Value::as_str) {
        if account_id == "@native" {
            let Some(mut model) =
                subscription_route_model(config, AUTO_REVIEW_MODEL_ID, None, |_| None)
            else {
                continue;
            };
            set_review_route_model(&mut model, requested_model);
            let mut provider = Map::from_iter([
                ("id".to_owned(), Value::String("codex-native".to_owned())),
                ("name".to_owned(), Value::String("Native Codex".to_owned())),
                (
                    "base_url".to_owned(),
                    config
                        .get("codex_base_url")
                        .cloned()
                        .unwrap_or_else(|| Value::String(DEFAULT_CODEX_BASE_URL.to_owned())),
                ),
                ("protocol".to_owned(), Value::String("responses".to_owned())),
                ("auth_mode".to_owned(), Value::String("forward".to_owned())),
                ("implicit_native".to_owned(), Value::Bool(true)),
            ]);
            if let Some(path) = config
                .get("_native_auth_path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
            {
                provider.insert(
                    "_native_auth_path".to_owned(),
                    Value::String(path.to_owned()),
                );
            }
            return Some(resolved_route_from_parts(
                requested_model,
                provider,
                model,
                RouteSource::ImplicitNative,
            ));
        }
        let Some(account) = accounts
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
            .find(|account| account.get("id").and_then(Value::as_str) == Some(account_id))
        else {
            continue;
        };
        if account.get("enabled") == Some(&Value::Bool(false))
            || !account.get("auth_file").is_some_and(is_python_truthy)
            || account.get("credential_status").and_then(Value::as_str) == Some("invalid")
        {
            continue;
        }
        let Some(mut model) =
            subscription_route_model(config, AUTO_REVIEW_MODEL_ID, Some(account), |account| {
                account_catalog_headers(account, &state.backend.configuration.vault)
            })
        else {
            continue;
        };
        set_review_route_model(&mut model, requested_model);
        let account_id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let provider = Map::from_iter([
            ("id".to_owned(), Value::String(account_id.to_owned())),
            (
                "name".to_owned(),
                account
                    .get("name")
                    .cloned()
                    .unwrap_or_else(|| Value::String(account_id.to_owned())),
            ),
            (
                "base_url".to_owned(),
                config
                    .get("codex_base_url")
                    .cloned()
                    .unwrap_or_else(|| Value::String(DEFAULT_CODEX_BASE_URL.to_owned())),
            ),
            ("protocol".to_owned(), Value::String("responses".to_owned())),
            ("auth_mode".to_owned(), Value::String("account".to_owned())),
            ("account".to_owned(), Value::Object(account.clone())),
        ]);
        return Some(resolved_route_from_parts(
            requested_model,
            provider,
            model,
            RouteSource::SubscriptionAccount,
        ));
    }
    None
}

fn set_review_route_model(model: &mut Map<String, Value>, requested_model: &str) {
    model.insert("id".to_owned(), Value::String(requested_model.to_owned()));
    model.insert(
        "upstream_id".to_owned(),
        Value::String(AUTO_REVIEW_MODEL_ID.to_owned()),
    );
    let context_window = model
        .get("context_window")
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(value) => value.parse::<i64>().ok(),
            Value::Bool(value) => Some(i64::from(*value)),
            _ => None,
        })
        .unwrap_or(0);
    if context_window > 0 {
        let mut sources = model
            .get("capability_sources")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !sources.get("context_window").is_some_and(Value::is_object) {
            sources.insert(
                "context_window".to_owned(),
                json!({"source":"official","confidence":0.95,"observed_at":null}),
            );
        }
        model.insert("capability_sources".to_owned(), Value::Object(sources));
    }
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|value| value.is_finite())
}

fn is_python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
