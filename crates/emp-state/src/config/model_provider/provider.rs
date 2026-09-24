//! One external provider record.

use super::*;

fn normalize_provider_capabilities(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Object(Map::new())),
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new("provider.capabilities must be an object"));
        }
    };
    let mut normalized = Map::new();
    for name in PROVIDER_BOOLEAN_CAPABILITIES {
        let Some(value) = raw.get(name) else {
            continue;
        };
        let Value::Bool(value) = value else {
            return Err(ConfigError::new(format!(
                "provider.capabilities.{name} must be boolean"
            )));
        };
        normalized.insert(name.to_owned(), Value::Bool(*value));
    }
    Ok(Value::Object(normalized))
}

/// Normalize one complete external-provider record into the persisted shape.
pub fn normalize_provider(raw: &Value) -> ConfigResult<Value> {
    let Value::Object(raw) = raw else {
        return Err(ConfigError::new("each provider must be an object"));
    };
    let id = normalize_provider_id(raw.get("id"))?;
    let name = string_value(raw.get("name"), "value", false)?;
    let name = if name.is_empty() {
        string_value(raw.get("id"), "provider.id", false)?
    } else {
        name
    };
    let base_url = normalize_provider_base_url(raw.get("base_url"))?;
    let protocol = string_value(raw.get("protocol"), "value", false)?;
    let protocol = if protocol.is_empty() {
        "chat_completions".to_owned()
    } else {
        protocol
    };
    let auth_mode = string_value(raw.get("auth_mode"), "value", false)?;
    let auth_mode = if auth_mode.is_empty() {
        "api_key".to_owned()
    } else {
        auth_mode
    };
    let api_key = string_value(raw.get("api_key"), "value", false)?;
    let api_key_file = string_value(raw.get("api_key_file"), "value", false)?;
    let anthropic_version = string_value(raw.get("anthropic_version"), "value", false)?;
    let anthropic_version = if anthropic_version.is_empty() {
        "2023-06-01".to_owned()
    } else {
        anthropic_version
    };
    let enabled = raw.get("enabled").is_none_or(json_truthy);
    let deployment_identity = safe_capability_identity(
        raw.get("deployment_identity"),
        "provider.deployment_identity",
    )?;
    let resolved_protocol = normalize_resolved_protocol(raw.get("resolved_protocol"))?;
    let protocol_observation = normalize_protocol_observation(raw.get("protocol_observation"))?;
    let capabilities = normalize_provider_capabilities(raw.get("capabilities"))?;

    if !PROVIDER_PROTOCOLS.contains(&protocol.as_str()) {
        return Err(ConfigError::new(
            "provider.protocol must be auto, responses, chat_completions, or anthropic_messages",
        ));
    }
    if !PROVIDER_AUTH_MODES.contains(&auth_mode.as_str()) {
        return Err(ConfigError::new(
            "provider.auth_mode must be api_key, anthropic_api_key, or forward",
        ));
    }
    if auth_mode == "forward" && protocol != "responses" {
        return Err(ConfigError::new(
            "forward providers must use the Responses protocol",
        ));
    }
    Ok(serde_json::json!({
        "id": id,
        "name": name,
        "base_url": base_url,
        "protocol": protocol,
        "auth_mode": auth_mode,
        "api_key": api_key,
        "api_key_file": api_key_file,
        "anthropic_version": anthropic_version,
        "enabled": enabled,
        "deployment_identity": deployment_identity,
        "resolved_protocol": resolved_protocol,
        "protocol_observation": protocol_observation,
        "capabilities": capabilities,
    }))
}
