//! Image account preferences are independent of model catalog visibility.
use super::{ConfigError, ConfigResult};
use serde_json::{Value, json};

pub(super) fn normalize(raw: Option<&Value>) -> ConfigResult<Value> {
    let Some(raw) = raw else {
        return Ok(json!({"mode":"follow", "account_id":""}));
    };
    let raw = raw
        .as_object()
        .ok_or_else(|| ConfigError::new("image_routing must be an object"))?;
    let mode = match raw.get("mode") {
        None => "follow",
        Some(Value::String(mode))
            if matches!(mode.as_str(), "follow" | "account" | "auto" | "credit") =>
        {
            mode
        }
        _ => {
            return Err(ConfigError::new(
                "image_routing.mode must be follow, account, auto or credit",
            ));
        }
    };
    let account = match raw.get("account_id") {
        None => "",
        Some(Value::String(account)) => account.trim(),
        _ => {
            return Err(ConfigError::new(
                "image_routing.account_id must be a string",
            ));
        }
    };
    if account.len() > 128 || account.chars().any(char::is_control) {
        return Err(ConfigError::new("image_routing.account_id is invalid"));
    }
    if mode == "account" && account.is_empty() {
        return Err(ConfigError::new("image_routing.account_id is required"));
    }
    let mut result =
        json!({"mode":mode, "account_id":if mode == "account" { account } else { "" }});
    if mode == "credit" || raw.get("credit").is_some_and(|credit| !credit.is_null()) {
        let credit = raw.get("credit").unwrap_or(&Value::Null);
        let provider = credit["provider"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128 && !super::is_team_service_id(id));
        let model = credit["model"]
            .as_str()
            .filter(|model| matches!(*model, "gpt-image-2.6-flare" | "gpt-image-2.6-sunburst"));
        let started = credit["cycle_started_at"].as_i64().filter(|date| *date > 0);
        let expires = credit["expires_at"]
            .as_i64()
            .filter(|date| *date <= 4_102_444_800);
        let budget = credit["budget_cents"]
            .as_i64()
            .filter(|amount| (1..=100_000_000).contains(amount));
        let percent = credit["percent"]
            .as_i64()
            .filter(|percent| (1..=100).contains(percent));
        match (provider, model, started, expires, budget, percent) {
            (
                Some(provider),
                Some(model),
                Some(started),
                Some(expires),
                Some(budget),
                Some(percent),
            ) if started < expires => {
                result["credit"] = json!({"provider":provider,"model":model,"cycle_started_at":started,"expires_at":expires,"budget_cents":budget,"percent":percent,"renew_monthly":credit["renew_monthly"] == true});
            }
            _ => {
                return Err(ConfigError::new(
                    "image_routing.credit requires a provider, model, allowance and valid billing period",
                ));
            }
        }
    }
    Ok(result)
}
