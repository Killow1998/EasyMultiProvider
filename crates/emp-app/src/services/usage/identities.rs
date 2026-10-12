//! Capture account labels while the exact credential headers are selected.
use crate::app::ServerState;
use base64::Engine;
use emp_core::{ResolvedRoute, RouteSource};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn remember(
    state: &ServerState,
    route: &ResolvedRoute,
    headers: &BTreeMap<String, String>,
) {
    let owner = emp_state::usage::account_owner(headers);
    if !owner.starts_with("account:") {
        return;
    }
    let account = route
        .provider
        .value()
        .get("account")
        .unwrap_or(&Value::Null);
    let (category, id, name) = match route.source {
        RouteSource::ImplicitNative => ("native", "@native", "Native"),
        RouteSource::SubscriptionAccount => (
            "subscription",
            account["id"].as_str().unwrap_or(&route.provider_id),
            account["name"].as_str().unwrap_or(&route.provider_id),
        ),
        _ => return,
    };
    let token = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, value)| value.strip_prefix("Bearer "))
        .and_then(|value| value.split('.').nth(1));
    let claims = token
        .and_then(|value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(value)
                .ok()
        })
        .and_then(|value| serde_json::from_slice::<Value>(&value).ok())
        .unwrap_or(Value::Null);
    let email = claims["email"]
        .as_str()
        .or_else(|| claims["https://api.openai.com/profile"]["email"].as_str());
    let masked = email
        .and_then(|value| value.split_once('@'))
        .map(|(local, domain)| format!("{}***@{domain}", local.chars().next().unwrap_or('*')))
        .unwrap_or_else(|| {
            account["quota"]["account_label"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        });
    let _ = state
        .backend
        .usage
        .ledger
        .remember_account(category, &owner, id, name, &masked);
}
