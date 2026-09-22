//! Python-compatible validation for subscription context-window preferences.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// The Python caller raises `ValueError`; its `str` value is represented here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionContextError(String);

impl SubscriptionContextError {
    pub const fn python_type(&self) -> &'static str {
        "ValueError"
    }

    fn exceeds_limit(slug: &str, maximum: u64) -> Self {
        Self(format!(
            "Context for {slug} exceeds the subscription catalog limit ({maximum} tokens); refresh models first",
        ))
    }
}

impl fmt::Display for SubscriptionContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SubscriptionContextError {}

/// Validate normalized application state against the current catalog limits.
pub fn validate_subscription_contexts(
    config: &Value,
    previous: Option<&Value>,
    native: &Value,
    account_catalogs: &BTreeMap<String, Value>,
) -> Result<(), SubscriptionContextError> {
    validate_source(config, previous, native, account_catalogs, None)?;
    for account in array(config, "accounts") {
        validate_source(config, previous, native, account_catalogs, Some(account))?;
    }
    Ok(())
}

fn validate_source(
    config: &Value,
    previous: Option<&Value>,
    native: &Value,
    account_catalogs: &BTreeMap<String, Value>,
    account: Option<&Value>,
) -> Result<(), SubscriptionContextError> {
    let field = if account.is_some() {
        "model_context_windows"
    } else {
        "native_model_context_windows"
    };
    let previous = previous.unwrap_or(&Value::Null);
    let windows = match account {
        Some(account) => account.get(field).and_then(Value::as_object),
        None => config.get(field).and_then(Value::as_object),
    };
    let Some(windows) = windows else {
        return Ok(());
    };
    let old_windows = match account {
        Some(account) => previous
            .get("accounts")
            .and_then(Value::as_array)
            .and_then(|accounts| {
                accounts.iter().find(|old| {
                    old.get("id").and_then(Value::as_str)
                        == account.get("id").and_then(Value::as_str)
                })
            })
            .and_then(|old| old.get(field).and_then(Value::as_object)),
        None => previous.get(field).and_then(Value::as_object),
    };
    let limits = subscription_limits(native, account_catalogs, account);
    for (slug, tokens) in windows {
        if old_windows.and_then(|old| old.get(slug)) == Some(tokens) {
            continue;
        }
        let Some(&maximum) = limits.get(slug) else {
            return Err(SubscriptionContextError::exceeds_limit(slug, 0));
        };
        if valid_context_limit(tokens).is_none_or(|requested| requested > maximum) {
            return Err(SubscriptionContextError::exceeds_limit(slug, maximum));
        }
    }
    Ok(())
}

fn subscription_limits(
    native: &Value,
    account_catalogs: &BTreeMap<String, Value>,
    account: Option<&Value>,
) -> BTreeMap<String, u64> {
    let catalog = match account {
        Some(account) => account
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| account_catalogs.get(id)),
        None => Some(native),
    };
    let Some(catalog) = catalog else {
        return BTreeMap::new();
    };
    crate::management_views::subscription_model_options(catalog)
        .into_iter()
        .filter_map(|option| {
            let id = option.get("id")?.as_str()?.to_owned();
            let maximum = option.get("max_context_window")?.as_u64()?;
            Some((id, maximum))
        })
        .collect()
}

fn valid_context_limit(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}
