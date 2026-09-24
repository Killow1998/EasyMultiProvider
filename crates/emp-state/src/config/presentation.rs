//! Browser-safe configuration projection and catalog controls.

use super::*;

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

/// Project normalized configuration into the credential-free browser shape.
///
/// Duplicate-account labels and secret-file status are supplied by the
/// composition layer so this transformation never reads or decrypts account
/// credentials and remains deterministic under differential tests.
pub fn public_configuration_with_file_status(
    config: &Value,
    duplicate_accounts: &BTreeMap<String, String>,
    secret_file_is_regular: impl Fn(&Path) -> bool,
) -> ConfigResult<Value> {
    let Value::Object(_) = config else {
        return Err(ConfigError::new("configuration must be a JSON object"));
    };
    let mut result = config.clone();
    let result_object = result
        .as_object_mut()
        .expect("configuration object checked above");

    let accounts = result_object
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut public_accounts = Vec::with_capacity(accounts.len());
    for raw in accounts {
        let account = normalize_account(&raw)
            .map_err(|error| ConfigError::python("AccountError", error.to_string()))?;
        let account = account
            .as_object()
            .expect("normalized account is always an object");
        let id = account
            .get("id")
            .and_then(Value::as_str)
            .expect("normalized account ID");
        let duplicate_of = duplicate_accounts.get(id).cloned().unwrap_or_default();
        public_accounts.push(serde_json::json!({
            "id": account["id"].clone(),
            "name": account["name"].clone(),
            "prefix": account["prefix"].clone(),
            "enabled": account["enabled"].clone(),
            "hidden_models": account["hidden_models"].clone(),
            "model_context_windows": account["model_context_windows"].clone(),
            "credential_set": account["auth_file"].as_str().is_some_and(|path| !path.is_empty()),
            "credential_status": account["credential_status"].clone(),
            "quota": account["quota"].clone(),
            "duplicate": !duplicate_of.is_empty(),
            "duplicate_of": duplicate_of,
        }));
    }
    result_object.insert("accounts".to_owned(), Value::Array(public_accounts));

    let providers = result_object
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ConfigError::new("configuration.providers must be a list"))?;
    for provider in providers {
        let provider = provider
            .as_object_mut()
            .ok_or_else(|| ConfigError::new("each provider must be an object"))?;
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
    Ok(result)
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
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | ':' | '-')
    }) {
        return Err(presentation_error(
            "catalog_presentations route contains unsupported characters",
        ));
    }
    Ok(route.to_owned())
}

/// Normalize route-keyed catalog presentation controls exactly like Python.
///
/// Python intentionally rebuilds each value from the three public fields, so
/// stale or unknown presentation fields are discarded here as well.
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

/// Normalize automatic subscription search exactly like Python.
pub fn normalize_subscription_search(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => None,
        Some(Value::Object(raw)) => Some(raw),
        Some(_) => {
            return Err(ConfigError::new("subscription_search must be an object"));
        }
    };
    let enabled = match raw.and_then(|value| value.get("enabled")) {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => {
            return Err(ConfigError::new(
                "subscription_search.enabled must be boolean",
            ));
        }
    };
    Ok(serde_json::json!({"enabled": enabled, "account_id": ""}))
}

/// Validate ordered runtime selection with Python's trimming and deduplication.
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
