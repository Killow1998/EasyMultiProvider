use super::*;

pub(super) fn normalize_memory_model<'a>(
    raw: Option<&'a Value>,
    field: &str,
) -> ConfigResult<&'a str> {
    match raw {
        None => Ok(""),
        Some(Value::String(model))
            if model.is_empty()
                || (model.starts_with("gpt-")
                    && model.len() <= 128
                    && model.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_')
                    })) =>
        {
            Ok(model)
        }
        _ => Err(ConfigError::new(format!(
            "{field} must be an unprefixed GPT model ID or empty"
        ))),
    }
}

pub(super) fn normalize_memory_effort<'a>(
    raw: Option<&'a Value>,
    field: &str,
) -> ConfigResult<&'a str> {
    match raw {
        None => Ok(""),
        Some(Value::String(effort))
            if effort.is_empty()
                || emp_domain::model_values::is_concrete_reasoning_effort(effort) =>
        {
            Ok(effort)
        }
        _ => Err(ConfigError::new(format!(
            "{field} must be a concrete reasoning effort or empty"
        ))),
    }
}

const MAX_CATALOG_ALIAS_BYTES: usize = 512;
const RUNTIME_SOURCES: [&str; 8] = [
    "auto",
    "configured",
    "codex_app",
    "managed",
    "vscode",
    "vscode_insiders",
    "cursor",
    "path_cli",
];

pub fn public_accounts(config: &Value, duplicate_accounts: &BTreeMap<String, String>) -> Value {
    let accounts = config["accounts"].as_array().expect("normalized accounts");
    Value::Array(
        accounts.iter().map(|account| {
            let id = account["id"].as_str().expect("normalized account ID");
            let duplicate_of = duplicate_accounts.get(id).map_or("", String::as_str);
            serde_json::json!({
                "id": id,
                "name": account["name"],
                "prefix": account["prefix"],
                "enabled": account["enabled"],
                "hidden_models": account["hidden_models"],
                "model_context_windows": account["model_context_windows"],
                "credential_set": !account["auth_file"].as_str().expect("normalized auth path").is_empty(),
                "credential_status": account["credential_status"],
                "quota": account["quota"],
                "duplicate": !duplicate_of.is_empty(),
                "duplicate_of": duplicate_of,
            })
        }).collect()
    )
}

pub fn public_configuration_with_file_status(
    config: &Value,
    duplicate_accounts: &BTreeMap<String, String>,
    secret_file_is_regular: impl Fn(&Path) -> bool,
) -> Value {
    let mut result = config.clone();
    result["accounts"] = public_accounts(config, duplicate_accounts);
    for provider in result["providers"]
        .as_array_mut()
        .expect("normalized providers")
    {
        let provider = provider.as_object_mut().expect("normalized provider");
        let key = provider.remove("api_key").unwrap_or(Value::Null);
        let secret_file = provider.remove("api_key_file").unwrap_or(Value::Null);
        let key_set = json_truthy(&key);
        let secret_set = secret_file
            .as_str()
            .filter(|path| !path.is_empty())
            .is_some_and(|path| secret_file_is_regular(Path::new(path)));
        provider.insert("api_key_set".to_owned(), Value::Bool(key_set || secret_set));
        provider.insert(
            "api_key".to_owned(),
            Value::String(if key_set { MASKED_API_KEY } else { "" }.to_owned()),
        );
    }
    result
}

fn presentation_error(message: &'static str) -> ConfigError {
    ConfigError::new(message)
}

fn normalize_route(raw: &str) -> ConfigResult<String> {
    let route = raw.trim();
    if route.is_empty() {
        return Err(presentation_error(
            "catalog_presentations route is required",
        ));
    }
    if !route.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | ':' | '-' | '@')
    }) {
        return Err(presentation_error(
            "catalog_presentations route contains unsupported characters",
        ));
    }
    Ok(route.to_owned())
}

/// Normalize route-keyed controls to the three public fields, discarding stale or unknown fields.
pub fn normalize_catalog_presentations(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Object(Map::new())),
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(presentation_error(
                "catalog_presentations must be an object",
            ));
        }
    };

    let mut result = Map::new();
    for (raw_route, raw_presentation) in raw {
        let route = normalize_route(raw_route)?;
        let Value::Object(presentation) = raw_presentation else {
            return Err(presentation_error("catalog presentation must be an object"));
        };
        let alias = match presentation.get("catalog_alias") {
            None => String::new(),
            Some(Value::String(value)) => value.clone(),
            Some(_) => return Err(presentation_error("catalog_alias must be a string")),
        };
        if alias.len() > MAX_CATALOG_ALIAS_BYTES {
            return Err(presentation_error("catalog_alias is too long"));
        }
        if alias
            .chars()
            .any(|character| (character as u32) < 32 || character as u32 == 127)
        {
            return Err(presentation_error(
                "catalog_alias contains unsupported characters",
            ));
        }
        let show_context = match presentation.get("show_context") {
            None => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(presentation_error("show_context must be boolean")),
        };
        let reasoning_summary = match presentation.get("reasoning_summary") {
            None => "auto".to_owned(),
            Some(Value::String(value)) => value.trim().to_lowercase(),
            Some(_) => {
                return Err(presentation_error("reasoning_summary must be a string"));
            }
        };
        if !REASONING_SUMMARIES.contains(&reasoning_summary.as_str()) {
            return Err(presentation_error(
                "reasoning_summary must be auto, show, or hide",
            ));
        }

        let mut normalized = Map::new();
        normalized.insert("catalog_alias".to_owned(), Value::String(alias));
        normalized.insert("show_context".to_owned(), Value::Bool(show_context));
        normalized.insert(
            "reasoning_summary".to_owned(),
            Value::String(reasoning_summary),
        );
        result.insert(route, Value::Object(normalized));
    }
    Ok(Value::Object(result))
}

/// Family-scoped Ultra overrides are independent of aliases and route display.
pub(super) fn normalize_ultra_efforts(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(serde_json::json!({})),
        Some(Value::Object(raw)) => raw,
        _ => {
            return Err(ConfigError::new(
                "ultra_reasoning_efforts must be an object",
            ));
        }
    };
    let mut result = Map::new();
    for (model, value) in raw {
        let model = normalize_route(model)?;
        let effort = value
            .as_str()
            .ok_or_else(|| {
                ConfigError::new("ultra_reasoning_efforts values must be concrete efforts or auto")
            })?
            .trim()
            .to_lowercase();
        if effort == "auto" {
            continue;
        }
        if !emp_domain::model_values::is_concrete_reasoning_effort(&effort) {
            return Err(ConfigError::new(
                "ultra_reasoning_efforts values must be concrete efforts or auto",
            ));
        }
        result.insert(model, Value::String(effort));
    }
    Ok(Value::Object(result))
}

/// Normalize subscription search, keeping explicit choices and enabling new settings.
pub fn normalize_subscription_search(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => None,
        Some(Value::Object(raw)) => Some(raw),
        Some(_) => {
            return Err(ConfigError::new("subscription_search must be an object"));
        }
    };
    let enabled = match raw.and_then(|value| value.get("enabled")) {
        None => true,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(ConfigError::new(
                "subscription_search.enabled must be boolean",
            ));
        }
    };
    Ok(serde_json::json!({"enabled": enabled, "account_id": ""}))
}

/// Map model names that have no public price to a model that has one.
/// Empty references drop the entry.
pub fn normalize_pricing_aliases(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Object(Map::new())),
        Some(Value::Object(raw)) => raw,
        Some(_) => return Err(ConfigError::new("pricing_aliases must be an object")),
    };
    if raw.len() > 1000 {
        return Err(ConfigError::new("pricing_aliases has too many entries"));
    }
    let mut aliases = Map::new();
    for (model, reference) in raw {
        let model = model.trim();
        let Some(reference) = reference.as_str() else {
            return Err(ConfigError::new("pricing_aliases values must be strings"));
        };
        let reference = reference.trim();
        if model.is_empty() || reference.is_empty() {
            continue;
        }
        if model.chars().count() > 256 || reference.chars().count() > 256 {
            return Err(ConfigError::new("pricing_aliases entries are too long"));
        }
        aliases.insert(model.to_owned(), Value::String(reference.to_owned()));
    }
    Ok(Value::Object(aliases))
}

/// Trim and deduplicate runtime selections while preserving order.
pub fn normalize_codex_runtime_sources(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => {
            return Ok(Value::Array(vec![Value::String("auto".to_owned())]));
        }
        Some(Value::Array(raw)) if !raw.is_empty() => raw,
        Some(_) => {
            return Err(ConfigError::new(
                "codex_runtime_sources must be a non-empty list",
            ));
        }
    };
    if raw.len() > RUNTIME_SOURCES.len() {
        return Err(ConfigError::new(
            "codex_runtime_sources has too many entries",
        ));
    }

    let mut sources = Vec::with_capacity(raw.len());
    for (index, value) in raw.iter().enumerate() {
        let source = match value {
            Value::Null => String::new(),
            Value::String(value) => value.trim().to_owned(),
            _ => {
                return Err(ConfigError::new(format!(
                    "codex_runtime_sources[{index}] must be a string"
                )));
            }
        };
        if !RUNTIME_SOURCES.contains(&source.as_str()) {
            return Err(ConfigError::new(
                "codex_runtime_sources contains an unsupported source",
            ));
        }
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    if sources.len() != 1 && sources.iter().any(|value| value == "auto") {
        return Err(ConfigError::new(
            "codex_runtime_sources auto cannot be combined",
        ));
    }
    Ok(Value::Array(
        sources.into_iter().map(Value::String).collect(),
    ))
}
