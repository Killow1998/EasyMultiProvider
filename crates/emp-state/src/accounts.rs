//! Exact, bounded Codex account metadata normalization.
//!
//! This slice intentionally follows the Python implementation rather than
//! inventing a new serialized schema. Unknown fields are discarded and quota
//! values are passed through as opaque JSON objects.

use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;

const MAX_HIDDEN_MODELS: usize = 1000;
const MAX_MODEL_ID_BYTES: usize = 256;
const CREDENTIAL_STATUSES: [&str; 3] = ["unknown", "valid", "invalid"];

/// A stable Python-visible account failure without credential material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountError {
    message: String,
}

impl AccountError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AccountError {}

pub type AccountResult<T> = Result<T, AccountError>;

/// Apply Python `str.strip()` semantics, including the four C0 file separators.
fn python_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(
            character,
            '\t'
                | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\r'
                | ' '
                | '\u{85}'
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{1c}'..='\u{1f}'
        )
    })
}

fn json_truthy(value: &Value) -> bool {
    if matches!(value, Value::Bool(false) | Value::Null) {
        return false;
    }
    if let Value::Number(number) = value {
        return number.as_f64() != Some(0.0);
    }
    !matches!(value, Value::String(value) if value.is_empty())
        && !matches!(value, Value::Array(values) if values.is_empty())
        && !matches!(value, Value::Object(values) if values.is_empty())
}

fn auth_file(raw: Option<&Value>) -> AccountResult<String> {
    match raw {
        None => Ok(String::new()),
        Some(Value::String(value)) => Ok(python_trim(value).to_owned()),
        Some(_) => Err(AccountError::new("account.auth_file must be a string")),
    }
}

/// Normalize, bound, deduplicate, and sort hidden model identifiers.
pub fn normalize_hidden_models(raw: Option<&Value>, field: &str) -> AccountResult<Vec<String>> {
    let Some(value) = raw else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let Value::Array(values) = value else {
        return Err(AccountError::new(format!("{field} must be a bounded list")));
    };
    if values.len() > MAX_HIDDEN_MODELS {
        return Err(AccountError::new(format!("{field} must be a bounded list")));
    }
    let mut model_ids = BTreeSet::new();
    for value in values {
        let Value::String(item) = value else {
            return Err(AccountError::new(format!("{field} must contain model IDs")));
        };
        let model_id = python_trim(item);
        if model_id.is_empty() {
            return Err(AccountError::new(format!("{field} must contain model IDs")));
        }
        if model_id.len() > MAX_MODEL_ID_BYTES {
            return Err(AccountError::new(format!(
                "{field} contains an oversized model ID"
            )));
        }
        model_ids.insert(model_id.to_owned());
    }
    Ok(model_ids.into_iter().collect())
}

/// Normalize model token limits while rejecting booleans and non-positive integers.
pub fn normalize_context_windows(raw: Option<&Value>) -> AccountResult<Map<String, Value>> {
    let Some(value) = raw else {
        return Ok(Map::new());
    };
    if value.is_null() {
        return Ok(Map::new());
    }
    let Value::Object(raw) = value else {
        return Err(AccountError::new(
            "model_context_windows must be a bounded object",
        ));
    };
    if raw.len() > MAX_HIDDEN_MODELS {
        return Err(AccountError::new(
            "model_context_windows must be a bounded object",
        ));
    }
    let mut normalized = Map::new();
    for (model, tokens) in raw {
        let trimmed_model = python_trim(model);
        if trimmed_model.is_empty() || model.len() > MAX_MODEL_ID_BYTES {
            return Err(AccountError::new(
                "model_context_windows has an invalid model ID",
            ));
        }
        let Value::Number(tokens) = tokens else {
            return Err(AccountError::new(
                "model_context_windows values must be positive integer tokens",
            ));
        };
        let Some(tokens) = tokens.as_u64() else {
            return Err(AccountError::new(
                "model_context_windows values must be positive integer tokens",
            ));
        };
        if tokens == 0 {
            return Err(AccountError::new(
                "model_context_windows values must be positive integer tokens",
            ));
        }
        normalized.insert(model.clone(), Value::from(tokens));
    }
    Ok(normalized)
}

/// Normalize one Codex account record into its persisted JSON shape.
pub fn normalize_account(raw: &Value) -> AccountResult<Value> {
    let Value::Object(raw) = raw else {
        return Err(AccountError::new("each account must be an object"));
    };
    let id = account_segment(raw.get("id"), "account.id")?;
    let prefix = account_segment(raw.get("prefix"), "account.prefix")?;
    let auth_file = auth_file(raw.get("auth_file"))?;
    let Some(credential_status) = raw
        .get("credential_status")
        .map_or_else(|| Some("unknown"), Value::as_str)
    else {
        return Err(AccountError::new("account.credential_status is invalid"));
    };
    if !CREDENTIAL_STATUSES.contains(&credential_status) {
        return Err(AccountError::new("account.credential_status is invalid"));
    }
    let name = account_name(raw.get("name"), &id)?;
    let enabled = raw.get("enabled").is_none_or(json_truthy);
    let hidden_models = normalize_hidden_models(raw.get("hidden_models"), "account.hidden_models")?;
    let model_context_windows = normalize_context_windows(raw.get("model_context_windows"))?;
    let quota = match raw.get("quota") {
        Some(Value::Object(quota)) => Value::Object(quota.clone()),
        _ => Value::Null,
    };

    Ok(serde_json::json!({
        "id": id,
        "name": name,
        "prefix": prefix,
        "auth_file": auth_file,
        "credential_status": credential_status,
        "enabled": enabled,
        "hidden_models": hidden_models,
        "model_context_windows": model_context_windows,
        "quota": quota,
    }))
}

fn account_segment(raw: Option<&Value>, field: &str) -> AccountResult<String> {
    let Some(Value::String(value)) = raw else {
        return Err(AccountError::new(format!(
            "{field} must be a safe single path segment"
        )));
    };
    let trimmed = python_trim(value);
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(AccountError::new(format!(
            "{field} must be a safe single path segment"
        )));
    }
    let valid = trimmed
        .chars()
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && trimmed.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        });
    if !valid {
        return Err(AccountError::new(format!(
            "{field} must be a safe single path segment"
        )));
    }
    Ok(trimmed.to_owned())
}

fn account_name(raw: Option<&Value>, fallback: &str) -> AccountResult<String> {
    match raw {
        None | Some(Value::Null) => Ok(fallback.to_owned()),
        Some(Value::String(value)) => {
            let name = python_trim(value);
            if name.is_empty() {
                Ok(fallback.to_owned())
            } else {
                Ok(name.to_owned())
            }
        }
        Some(_) => Err(AccountError::new("account.name must be a string")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_bool_and_non_positive_context_windows() {
        for input in [json!(true), json!(false), json!(0), json!(-1)] {
            assert_eq!(
                normalize_context_windows(Some(&json!({"model": input})))
                    .expect_err("invalid context window")
                    .to_string(),
                "model_context_windows values must be positive integer tokens"
            );
        }
    }
}
