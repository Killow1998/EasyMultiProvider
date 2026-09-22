//! Usage persistence and public price estimates; never store request content.
mod decimal;
pub mod ledger;
pub mod pricing;
use serde_json::Value;
pub const TOKEN_FIELDS: [&str; 6] = [
    "input_tokens",
    "output_tokens",
    "cached_input_tokens",
    "cache_write_tokens",
    "cache_write_1h_tokens",
    "reasoning_tokens",
];
pub const CATEGORIES: [&str; 4] = ["native", "subscription", "external", "unknown"];
pub fn token_count(value: Option<&Value>) -> Option<u64> {
    value?.as_u64().filter(|value| *value <= 10_000_000)
}
fn python_json(value: &Value, spaces: bool) -> String {
    use std::fmt::Write;
    let source = value.to_string();
    let mut result = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in source.chars() {
        if character >= '\u{7f}' {
            for unit in character.encode_utf16(&mut [0; 2]).iter() {
                write!(result, "\\u{unit:04x}").expect("JSON string");
            }
        } else {
            result.push(character);
        }
        if quoted {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quoted = false;
            }
        } else if character == '"' {
            quoted = true;
        } else if spaces && matches!(character, ',' | ':') {
            result.push(' ');
        }
    }
    result
}

pub fn account_owner(headers: &std::collections::BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let header = |wanted: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
            .map(|(_, value)| value.as_str())
    };
    if let Some(value) = header("chatgpt-account-id")
        .filter(|value| !value.trim().is_empty() && value.chars().count() <= 512)
    {
        return format!(
            "account:{:x}",
            Sha256::digest([b"emp-usage-account\0".as_slice(), value.trim().as_bytes()].concat())
        );
    }
    if let Some(value) = header("authorization").filter(|value| !value.trim().is_empty()) {
        return format!(
            "credential:{:x}",
            Sha256::digest(
                [
                    b"emp-usage-credential\0".as_slice(),
                    value.trim().as_bytes()
                ]
                .concat()
            )
        );
    }
    String::new()
}

pub fn reported_usage(event: &Value) -> serde_json::Map<String, Value> {
    let response = event
        .get("response")
        .filter(|value| value.is_object())
        .unwrap_or(event);
    let Some(usage) = response.get("usage").filter(|value| value.is_object()) else {
        return serde_json::Map::new();
    };
    let mut result = serde_json::Map::new();
    if let Some(total) = token_count(usage.get("input_tokens")) {
        result.insert("input_tokens".into(), total.into());
        if let Some(cached) = token_count(usage["input_tokens_details"].get("cached_tokens"))
            .filter(|cached| *cached <= total)
        {
            result.insert("cached_input_tokens".into(), cached.into());
        }
    }
    if let Some(output) = token_count(usage.get("output_tokens")) {
        result.insert("output_tokens".into(), output.into());
    }
    if let Some(reasoning) = token_count(usage["output_tokens_details"].get("reasoning_tokens")) {
        result.insert("reasoning_tokens".into(), reasoning.into());
    }
    for (source, target) in [
        ("cache_creation_tokens", "cache_write_tokens"),
        ("cache_creation_1h_tokens", "cache_write_1h_tokens"),
    ] {
        if let Some(value) = usage["input_tokens_details"].get(source) {
            result.insert(target.into(), serde_json::json!(token_count(Some(value))));
        }
    }
    if let Some(id) = response["id"]
        .as_str()
        .filter(|value| !value.is_empty() && value.chars().count() <= 256)
    {
        result.insert("usage_response_id".into(), id.into());
    }
    if let Some(tier) = response["service_tier"]
        .as_str()
        .filter(|value| !value.is_empty() && value.chars().count() <= 32)
    {
        result.insert("service_tier".into(), tier.into());
    }
    result
}

pub fn identity_hash(value: &Value) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(python_json(value, true).as_bytes()))
}
