//! Exact, bounded configuration helpers shared with the Python implementation.
//!
//! Full account, provider, model, path and persistence normalization remains a
//! later migration slice. Exposing only completed helpers prevents callers from
//! mistaking a partial configuration rebuild for the production contract.

use crate::model_values::{
    input_modalities_known, output_modalities_known, supported_protocols_known,
};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;

const MAX_CATALOG_ALIAS_BYTES: usize = 512;
const REASONING_SUMMARIES: [&str; 3] = ["auto", "show", "hide"];
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

/// A stable Python-visible configuration failure without private input data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

pub type ConfigResult<T> = Result<T, ConfigError>;

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

#[derive(Debug)]
struct ParsedProviderUrl<'a> {
    scheme: String,
    netloc: &'a str,
    path: &'a str,
    params: &'a str,
    query: &'a str,
    fragment: &'a str,
}

fn split_scheme(value: &str) -> Option<(&str, &str)> {
    let (scheme, remainder) = value.split_once(':')?;
    let mut characters = scheme.chars();
    if !characters.next()?.is_ascii_alphabetic()
        || !characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
    {
        return None;
    }
    Some((scheme, remainder))
}

fn split_path_params(path: &str) -> (&str, &str) {
    let search_from = path.rfind('/').map_or(0, |index| index + 1);
    match path[search_from..].find(';') {
        Some(relative) => {
            let index = search_from + relative;
            (&path[..index], &path[index + 1..])
        }
        None => (path, ""),
    }
}

fn parse_provider_url(value: &str) -> Option<ParsedProviderUrl<'_>> {
    let (scheme, remainder) = split_scheme(value)?;
    let authority_and_rest = remainder.strip_prefix("//")?;
    let authority_end = authority_and_rest
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_rest.len());
    let netloc = &authority_and_rest[..authority_end];
    let rest = &authority_and_rest[authority_end..];
    let (before_fragment, fragment) = rest
        .split_once('#')
        .map_or((rest, ""), |(head, tail)| (head, tail));
    let (path_with_params, query) = before_fragment
        .split_once('?')
        .map_or((before_fragment, ""), |(head, tail)| (head, tail));
    let (path, params) = split_path_params(path_with_params);
    Some(ParsedProviderUrl {
        scheme: scheme.to_ascii_lowercase(),
        netloc,
        path,
        params,
        query,
        fragment,
    })
}

fn userinfo(netloc: &str) -> (&str, Option<&str>) {
    let Some((raw, _)) = netloc.rsplit_once('@') else {
        return ("", None);
    };
    raw.split_once(':')
        .map_or((raw, None), |(username, password)| {
            (username, Some(password))
        })
}

fn hostname(netloc: &str) -> Option<&str> {
    let host_port = netloc
        .rsplit_once('@')
        .map_or(netloc, |(_, host_port)| host_port);
    if let Some(bracketed) = host_port.strip_prefix('[') {
        return bracketed.split_once(']').map(|(host, _)| host);
    }
    Some(
        host_port
            .rsplit_once(':')
            .map_or(host_port, |(host, _)| host),
    )
}

fn string_value(raw: Option<&Value>, field: &str, required: bool) -> ConfigResult<String> {
    let value = match raw {
        None | Some(Value::Null) => "",
        Some(Value::String(value)) => value.trim(),
        Some(_) => return Err(ConfigError::new(format!("{field} must be a string"))),
    };
    if required && value.is_empty() {
        return Err(ConfigError::new(format!("{field} is required")));
    }
    Ok(value.to_owned())
}

/// Validate and trim an external provider identifier exactly like Python.
pub fn normalize_provider_id(raw: Option<&Value>) -> ConfigResult<String> {
    let value = string_value(raw, "provider.id", true)?;
    let mut characters = value.chars();
    let valid = value.len() <= 64
        && characters
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        });
    if !valid {
        return Err(ConfigError::new(
            "provider.id must be a safe single path segment",
        ));
    }
    Ok(value)
}

fn strip_terminal_path<'a>(path: &'a str, suffix: &str) -> Option<&'a str> {
    if path.len() < suffix.len() {
        return None;
    }
    let root_end = path.len() - suffix.len();
    if !path.get(root_end..)?.eq_ignore_ascii_case(suffix) {
        return None;
    }
    path.get(..root_end)
}

fn normalize_provider_path(path: &str) -> String {
    let mut path = path.trim_end_matches('/').to_owned();
    for suffix in [
        "/responses/compact",
        "/chat/completions",
        "/response",
        "/responses",
        "/messages",
        "/models",
    ] {
        if let Some(root) = strip_terminal_path(&path, suffix) {
            path.truncate(root.len());
            break;
        }
    }

    let mut prefix_end = path.len();
    let mut v1_count = 0;
    while prefix_end >= 3
        && path
            .get(prefix_end - 3..prefix_end)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("/v1"))
    {
        prefix_end -= 3;
        v1_count += 1;
    }
    if v1_count >= 2 {
        path.truncate(prefix_end);
        path.push_str("/v1");
    }
    if path.is_empty() {
        path.push_str("/v1");
    }
    path
}

/// Turn a pasted provider origin or request URL into the API root EMP owns.
pub fn normalize_provider_base_url(raw: Option<&Value>) -> ConfigResult<String> {
    let entered = string_value(raw, "provider.base_url", true)?;
    let entered = entered.trim_end_matches('/');
    let parsed = parse_provider_url(entered)
        .ok_or_else(|| ConfigError::new("provider.base_url must be an http(s) URL"))?;
    if !matches!(parsed.scheme.as_str(), "http" | "https") || parsed.netloc.is_empty() {
        return Err(ConfigError::new("provider.base_url must be an http(s) URL"));
    }
    let (username, password) = userinfo(parsed.netloc);
    if !username.is_empty() || password.is_some_and(|value| !value.is_empty()) {
        return Err(ConfigError::new(
            "provider.base_url must not contain URL credentials",
        ));
    }
    if !parsed.query.is_empty() || !parsed.fragment.is_empty() {
        return Err(ConfigError::new(
            "provider.base_url must not contain a query or fragment",
        ));
    }
    let host = hostname(parsed.netloc).unwrap_or("").to_ascii_lowercase();
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if parsed.scheme == "http" && !loopback {
        return Err(ConfigError::new(
            "provider.base_url must use HTTPS unless it targets loopback",
        ));
    }

    let path = normalize_provider_path(parsed.path);
    let mut normalized = format!("{}://{}{}", parsed.scheme, parsed.netloc, path);
    if !parsed.params.is_empty() {
        normalized.push(';');
        normalized.push_str(parsed.params);
    }
    Ok(normalized.trim_end_matches('/').to_owned())
}

const PROVIDER_PROTOCOLS: [&str; 4] = [
    "auto",
    "responses",
    "chat_completions",
    "anthropic_messages",
];
const PROVIDER_AUTH_MODES: [&str; 3] = ["api_key", "anthropic_api_key", "forward"];
const CAPABILITY_SOURCES: [(&str, f64); 7] = [
    ("official", 0.95),
    ("advertised", 0.75),
    ("observed", 1.0),
    ("manual", 1.0),
    ("inherited", 0.6),
    ("inferred", 0.35),
    ("unknown", 0.0),
];
const PROVIDER_BOOLEAN_CAPABILITIES: [&str; 8] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "websocket",
];

const MODEL_CAPABILITY_SOURCE_FIELDS: [&str; 17] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "websocket",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "supports_image_detail_original",
];
const MODEL_EXPLICIT_CAPABILITY_FIELDS: [&str; 10] = [
    "supports_reasoning",
    "supports_reasoning_summaries",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "reasoning_control",
    "max_input_tokens",
    "structured_output",
    "web_search",
    "supports_image_detail_original",
];
const MODEL_BOOLEAN_CAPABILITIES: [&str; 8] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "websocket",
];

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn safe_capability_identity(raw: Option<&Value>, field: &str) -> ConfigResult<String> {
    let value = string_value(raw, field, false)?;
    let valid = value.len() <= 256
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | ':' | '-')
        });
    if !value.is_empty() && !valid {
        return Err(ConfigError::new(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(value)
}

fn normalize_resolved_protocol(raw: Option<&Value>) -> ConfigResult<String> {
    let value = string_value(raw, "resolved_protocol", false)?;
    if !value.is_empty() && !PROVIDER_PROTOCOLS[1..].contains(&value.as_str()) {
        return Err(ConfigError::new(
            "resolved_protocol must be a concrete protocol",
        ));
    }
    Ok(value)
}

fn leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn valid_iso_date(value: &str) -> bool {
    if value.len() != 10
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
    {
        return false;
    }
    let Ok(year) = value[..4].parse::<u32>() else {
        return false;
    };
    if year == 0 {
        return false;
    }
    let Ok(month) = value[5..7].parse::<u32>() else {
        return false;
    };
    let Ok(day) = value[8..].parse::<u32>() else {
        return false;
    };
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn two_digit_number(value: &str, maximum: u32) -> bool {
    value.len() == 2
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u32>().is_ok_and(|number| number <= maximum)
}

fn strip_fraction(value: &str) -> Option<&str> {
    let separator = value.find(['.', ',']);
    match separator {
        None => Some(value),
        Some(index) => {
            let fraction = &value[index + 1..];
            (!fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()))
                .then_some(&value[..index])
        }
    }
}

fn valid_iso_offset(value: &str) -> bool {
    let Some(value) = value.strip_prefix(['+', '-']) else {
        return false;
    };
    let mut parts = value.split(':');
    let Some(hour) = parts.next() else {
        return false;
    };
    let Some(minute) = parts.next() else {
        return false;
    };
    if !two_digit_number(hour, 23) || !two_digit_number(minute, 59) {
        return false;
    }
    match parts.next() {
        None => true,
        Some(second) => {
            let Some(second) = strip_fraction(second) else {
                return false;
            };
            two_digit_number(second, 59) && parts.next().is_none()
        }
    }
}

fn valid_iso_time(value: &str) -> bool {
    let (time, offset) = if let Some(time) = value.strip_suffix('Z') {
        (time, None)
    } else if let Some(index) = value
        .char_indices()
        .skip(1)
        .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))
    {
        (&value[..index], Some(&value[index..]))
    } else {
        (value, None)
    };
    if offset.is_some_and(|value| !valid_iso_offset(value)) {
        return false;
    }
    let mut parts = time.split(':');
    let Some(hour) = parts.next() else {
        return false;
    };
    if !two_digit_number(hour, 23) {
        return false;
    }
    let Some(minute) = parts.next() else {
        return true;
    };
    if !two_digit_number(minute, 59) {
        return false;
    }
    let Some(second) = parts.next() else {
        return true;
    };
    let Some(second) = strip_fraction(second) else {
        return false;
    };
    two_digit_number(second, 59) && parts.next().is_none()
}

fn valid_python_iso_timestamp(value: &str) -> bool {
    let Some(date) = value.get(..10) else {
        return false;
    };
    if !valid_iso_date(date) {
        return false;
    }
    let Some(rest) = value.get(10..) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let mut characters = rest.chars();
    let Some(_) = characters.next() else {
        return false;
    };
    valid_iso_time(characters.as_str())
}

fn protocol_confidence(raw: Option<&Value>, default: f64) -> ConfigResult<f64> {
    let value = match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => f64::from(*value),
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| ConfigError::new("invalid protocol_observation"))?,
        Some(Value::String(value)) => value
            .trim()
            .parse::<f64>()
            .map_err(|_| ConfigError::new("invalid protocol_observation"))?,
        Some(_) => return Err(ConfigError::new("invalid protocol_observation")),
    };
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::new("invalid protocol_observation"));
    }
    Ok(value)
}

fn normalize_protocol_observation(raw: Option<&Value>) -> ConfigResult<Value> {
    let empty = Map::new();
    let raw = match raw {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new("protocol_observation must be an object"));
        }
    };
    let source = match raw.get("source") {
        None => "unknown",
        Some(Value::String(value)) => value,
        Some(_) => return Err(ConfigError::new("invalid protocol_observation")),
    };
    let Some((_, default_confidence)) = CAPABILITY_SOURCES
        .iter()
        .find(|(candidate, _)| *candidate == source)
    else {
        return Err(ConfigError::new("invalid protocol_observation"));
    };
    let confidence = protocol_confidence(raw.get("confidence"), *default_confidence)?;
    let observed_at = match raw.get("observed_at") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(value)) if valid_python_iso_timestamp(value) => {
            Value::String(value.clone())
        }
        Some(_) => return Err(ConfigError::new("invalid protocol_observation")),
    };
    let fingerprint = string_value(
        raw.get("endpoint_fingerprint"),
        "protocol_observation.endpoint_fingerprint",
        false,
    )?;
    let valid_fingerprint = fingerprint.len() == 71
        && fingerprint.starts_with("sha256:")
        && fingerprint[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !fingerprint.is_empty() && !valid_fingerprint {
        return Err(ConfigError::new(
            "protocol_observation.endpoint_fingerprint is invalid",
        ));
    }
    Ok(serde_json::json!({
        "source": source,
        "confidence": confidence,
        "observed_at": observed_at,
        "endpoint_fingerprint": fingerprint,
        "deployment_identity": safe_capability_identity(
            raw.get("deployment_identity"),
            "protocol_observation.deployment_identity",
        )?,
        "upstream_model": safe_capability_identity(
            raw.get("upstream_model"),
            "protocol_observation.upstream_model",
        )?,
    }))
}

fn python_capability_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(
            character,
            '\t'
                | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\u{d}'
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

fn effective_model_capability_value<'a>(values: &'a Value, field: &str) -> Option<&'a Value> {
    if let Some(value) = values.get(field) {
        return Some(value);
    }
    values
        .get("capabilities")
        .filter(|value| value.is_object())
        .and_then(|capabilities| capabilities.get(field))
}

fn model_capability_known(field: &str, value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if MODEL_BOOLEAN_CAPABILITIES.contains(&field) {
        return value.is_boolean();
    }
    match field {
        "reasoning_levels" => value.as_array().is_some_and(|levels| !levels.is_empty()),
        "reasoning_control" => value
            .as_str()
            .is_some_and(|control| !python_capability_trim(control).is_empty()),
        "input_modalities" => input_modalities_known(Some(value)),
        "output_modalities" => output_modalities_known(Some(value)),
        "supported_protocols" => supported_protocols_known(Some(value)),
        "supports_image_detail_original" => value.is_boolean(),
        _ => {
            value.as_i64().is_some_and(|number| number > 0)
                || value.as_u64().is_some_and(|number| number > 0)
        }
    }
}

fn default_model_capability_source(
    field: &str,
    values: &Value,
    explicit_fields: Option<&[&str]>,
) -> &'static str {
    if MODEL_EXPLICIT_CAPABILITY_FIELDS.contains(&field) {
        return if explicit_fields.is_some_and(|fields| fields.contains(&field)) {
            "manual"
        } else {
            "unknown"
        };
    }
    if model_capability_known(field, effective_model_capability_value(values, field)) {
        "inferred"
    } else {
        "unknown"
    }
}

fn model_provenance_confidence(raw: Option<&Value>, default: f64) -> ConfigResult<f64> {
    let value = match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => f64::from(*value),
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| ConfigError::new("invalid provenance value"))?,
        Some(Value::String(value)) => value
            .trim()
            .parse::<f64>()
            .map_err(|_| ConfigError::new("invalid provenance value"))?,
        Some(_) => return Err(ConfigError::new("invalid provenance value")),
    };
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::new("invalid provenance value"));
    }
    Ok(value)
}

fn normalize_model_provenance(
    raw: &Map<String, Value>,
    default_source: &str,
) -> ConfigResult<Value> {
    let source = match raw.get("source") {
        None => default_source,
        Some(Value::String(source)) => source,
        Some(_) => return Err(ConfigError::new("unsupported capability source")),
    };
    let Some((_, default_confidence)) = CAPABILITY_SOURCES
        .iter()
        .find(|(candidate, _)| candidate == &source)
    else {
        return Err(ConfigError::new("unsupported capability source"));
    };
    let confidence = model_provenance_confidence(raw.get("confidence"), *default_confidence)?;
    let observed_at = match raw.get("observed_at") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(value)) if valid_python_iso_timestamp(value) => {
            Value::String(value.clone())
        }
        Some(_) => return Err(ConfigError::new("invalid capability observed_at")),
    };
    Ok(serde_json::json!({
        "source": source,
        "confidence": confidence,
        "observed_at": observed_at,
    }))
}

/// Normalize capability provenance for an already-normalized model record.
///
/// `values` must have the shape produced by Python `_normalize_model`; in
/// particular, top-level capability values take precedence over the nested
/// `capabilities` object even when the top-level value is null. Explicitness is
/// raw model-key presence and is supplied by the caller.
pub fn normalize_model_capability_sources(
    raw: Option<&Value>,
    values: &Value,
    explicit_fields: Option<&[&str]>,
) -> ConfigResult<Value> {
    if !values.is_object() {
        return Err(ConfigError::new(
            "model.capability_sources requires normalized model values",
        ));
    }
    let empty = Map::new();
    let raw = match raw {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(
                "model.capability_sources must be an object",
            ));
        }
    };
    let mut result = Map::new();
    for field in MODEL_CAPABILITY_SOURCE_FIELDS {
        let Some(value) = raw.get(field) else {
            if model_capability_known(field, effective_model_capability_value(values, field)) {
                let source = default_model_capability_source(field, values, explicit_fields);
                let provenance = normalize_model_provenance(&Map::new(), source).map_err(|_| {
                    ConfigError::new(format!("invalid provenance for model.{field}"))
                })?;
                result.insert(field.to_owned(), provenance);
            }
            continue;
        };
        let provenance = if let Some(object) = value.as_object() {
            let source = default_model_capability_source(field, values, explicit_fields);
            normalize_model_provenance(object, source)
                .map_err(|_| ConfigError::new(format!("invalid provenance for model.{field}")))?
        } else {
            return Err(ConfigError::new(format!(
                "model.capability_sources.{field} must be an object"
            )));
        };
        result.insert(field.to_owned(), provenance);
    }
    Ok(Value::Object(result))
}

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

fn python_exponent(mantissa: &str, exponent: i32) -> String {
    format!("{mantissa}e{exponent:+03}")
}

fn python_float_token(rendered: &str) -> String {
    if let Some((mantissa, exponent)) = rendered.split_once('e') {
        if let Ok(exponent) = exponent.parse::<i32>() {
            return python_exponent(mantissa, exponent);
        }
        return rendered.to_owned();
    }

    let (sign, unsigned) = rendered
        .strip_prefix('-')
        .map_or(("", rendered), |value| ("-", value));
    let Some(fraction) = unsigned.strip_prefix("0.") else {
        return rendered.to_owned();
    };
    let Some(first_nonzero) = fraction.bytes().position(|value| value != b'0') else {
        return rendered.to_owned();
    };
    let exponent = -(first_nonzero as i32) - 1;
    if exponent >= -4 {
        return rendered.to_owned();
    }
    let digits = &fraction[first_nonzero..];
    let mantissa = if digits.len() == 1 {
        format!("{sign}{digits}")
    } else {
        format!("{sign}{}.{}", &digits[..1], &digits[1..])
    };
    python_exponent(&mantissa, exponent)
}

fn write_python_json(value: &Value, output: &mut Vec<u8>) -> ConfigResult<()> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => {
            let rendered = number.to_string();
            if number.is_f64() {
                output.extend_from_slice(python_float_token(&rendered).as_bytes());
            } else {
                output.extend_from_slice(rendered.as_bytes());
            }
        }
        Value::String(value) => serde_json::to_writer(output, value)
            .map_err(|_| ConfigError::new("catalog cannot be canonically encoded"))?,
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_python_json(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(entries) => {
            output.push(b'{');
            let mut sorted = entries.iter().collect::<Vec<_>>();
            sorted.sort_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .map_err(|_| ConfigError::new("catalog cannot be canonically encoded"))?;
                output.push(b':');
                write_python_json(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

/// Return the exact compact UTF-8 JSON bytes hashed by Python's catalog ETag.
pub fn canonical_catalog_json(value: &Value) -> ConfigResult<Vec<u8>> {
    let mut output = Vec::new();
    write_python_json(value, &mut output)?;
    Ok(output)
}

/// Return the Python-compatible quoted catalog ETag.
pub fn catalog_etag(catalog: &Value) -> ConfigResult<String> {
    let digest = Sha256::digest(canonical_catalog_json(catalog)?);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(format!("\"emp-{encoded}\""))
}
