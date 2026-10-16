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
        Some(Value::String(mode)) if matches!(mode.as_str(), "follow" | "account" | "auto") => mode,
        _ => {
            return Err(ConfigError::new(
                "image_routing.mode must be follow, account or auto",
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
    Ok(json!({"mode":mode, "account_id":if mode == "account" { account } else { "" }}))
}
