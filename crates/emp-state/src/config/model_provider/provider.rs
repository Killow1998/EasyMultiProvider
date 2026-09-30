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
    let claude_login = raw.get("execution_backend").and_then(Value::as_str) == Some("claude_cli")
        && raw.get("auth_mode").and_then(Value::as_str) == Some("claude_login");
    let base_url = if claude_login {
        let base_url = string_value(raw.get("base_url"), "provider.base_url", false)?;
        if !base_url.is_empty() {
            return Err(ConfigError::new(
                "provider.base_url must be empty for Claude CLI local login",
            ));
        }
        String::new()
    } else {
        normalize_provider_base_url(raw.get("base_url"))?
    };
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
    let execution_backend = string_value(raw.get("execution_backend"), "value", false)?;
    let execution_backend = if execution_backend.is_empty() {
        "http"
    } else {
        execution_backend.as_str()
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
            "provider.auth_mode must be api_key, anthropic_api_key, forward, or claude_login",
        ));
    }
    if auth_mode == "forward" && protocol != "responses" {
        return Err(ConfigError::new(
            "forward providers must use the Responses protocol",
        ));
    }
    if auth_mode == "forward" && execution_backend == "claude_cli" {
        return Err(ConfigError::new(
            "provider.execution_backend claude_cli cannot use forward authentication",
        ));
    }
    if !["http", "claude_cli"].contains(&execution_backend) {
        return Err(ConfigError::new(
            "provider.execution_backend must be http or claude_cli",
        ));
    }
    if auth_mode == "claude_login" && execution_backend != "claude_cli" {
        return Err(ConfigError::new(
            "provider.auth_mode claude_login requires provider.execution_backend claude_cli",
        ));
    }
    if claude_login && (!api_key.is_empty() || !api_key_file.is_empty()) {
        return Err(ConfigError::new(
            "Claude CLI local login cannot include an API key or key file",
        ));
    }
    if execution_backend == "claude_cli"
        && !["auto", "anthropic_messages"].contains(&protocol.as_str())
    {
        return Err(ConfigError::new(
            "provider.execution_backend claude_cli requires provider.protocol auto or anthropic_messages",
        ));
    }
    let mut normalized = serde_json::json!({
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
    });
    // Preserve the historic serialized shape for providers using the default.
    // An explicit backend remains visible and round-trips through normal config APIs.
    if raw.contains_key("execution_backend") {
        normalized["execution_backend"] = Value::String(execution_backend.to_owned());
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::normalize_provider;
    use serde_json::json;

    #[test]
    fn execution_backend_defaults_to_http_without_changing_legacy_shape() {
        let provider = json!({
            "id":"example", "name":"Example", "base_url":"https://example.invalid/v1",
            "protocol":"responses", "auth_mode":"api_key", "api_key":"secret"
        });
        let normalized = normalize_provider(&provider).expect("legacy provider");

        assert!(normalized.get("execution_backend").is_none());
    }

    #[test]
    fn execution_backend_accepts_claude_cli_and_rejects_unknown_values() {
        let provider = json!({
            "id":"example", "name":"Example", "base_url":"https://example.invalid/v1",
            "protocol":"anthropic_messages", "auth_mode":"api_key", "api_key":"secret",
            "execution_backend":"claude_cli"
        });
        let normalized = normalize_provider(&provider).expect("Claude CLI provider");
        assert_eq!(normalized["execution_backend"], "claude_cli");

        let automatic = json!({
            "id":"automatic", "name":"Automatic", "base_url":"https://example.invalid/v1",
            "protocol":"auto", "auth_mode":"api_key", "api_key":"secret",
            "execution_backend":"claude_cli"
        });
        assert_eq!(
            normalize_provider(&automatic).expect("automatic Messages route")["protocol"],
            "auto"
        );

        let invalid = json!({
            "id":"example", "name":"Example", "base_url":"https://example.invalid/v1",
            "protocol":"responses", "auth_mode":"api_key", "api_key":"secret",
            "execution_backend":"shell"
        });
        assert_eq!(
            normalize_provider(&invalid)
                .expect_err("unknown execution backend")
                .to_string(),
            "provider.execution_backend must be http or claude_cli"
        );

        let unsupported_protocol = json!({
            "id":"example", "name":"Example", "base_url":"https://example.invalid/v1",
            "protocol":"responses", "auth_mode":"api_key", "api_key":"secret",
            "execution_backend":"claude_cli"
        });
        assert_eq!(
            normalize_provider(&unsupported_protocol)
                .expect_err("Claude CLI requires a Messages route")
                .to_string(),
            "provider.execution_backend claude_cli requires provider.protocol auto or anthropic_messages"
        );
    }

    #[test]
    fn claude_cli_local_login_requires_empty_url_and_no_provider_credentials() {
        let provider = json!({
            "id":"local-claude",
            "protocol":"anthropic_messages",
            "execution_backend":"claude_cli",
            "auth_mode":"claude_login"
        });
        let normalized = normalize_provider(&provider).expect("local CLI login");
        assert_eq!(normalized["execution_backend"], "claude_cli");
        assert_eq!(normalized["auth_mode"], "claude_login");
        assert_eq!(normalized["base_url"], "");
        assert_eq!(normalized["api_key"], "");
        assert_eq!(normalized["api_key_file"], "");

        let mut empty_url = provider.clone();
        empty_url["base_url"] = json!("");
        normalize_provider(&empty_url).expect("empty local URL");

        for (field, value, error) in [
            (
                "base_url",
                json!("https://cpa.example/v1"),
                "provider.base_url must be empty",
            ),
            ("api_key", json!("credential"), "cannot include an API key"),
            (
                "api_key_file",
                json!("secrets/provider.key.enc"),
                "cannot include an API key or key file",
            ),
        ] {
            let mut invalid = provider.clone();
            invalid[field] = value;
            assert!(
                normalize_provider(&invalid)
                    .expect_err("invalid local CLI provider")
                    .to_string()
                    .contains(error),
                "field {field} must be rejected"
            );
        }

        let mut http_local_login = provider;
        http_local_login["execution_backend"] = json!("http");
        http_local_login["base_url"] = json!("https://api.example/v1");
        assert!(
            normalize_provider(&http_local_login)
                .expect_err("local login requires the Claude CLI backend")
                .to_string()
                .contains("requires provider.execution_backend claude_cli")
        );
    }
}
