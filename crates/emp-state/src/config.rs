//! Exact, bounded configuration helpers shared with the Python implementation.
//!
//! Pure account, provider, model and top-level configuration normalization is
//! implemented here. Filesystem canonicalization, import merging and persistence
//! remain separate state transitions so callers cannot mistake parsing for I/O.

use crate::accounts::{normalize_account, normalize_context_windows, normalize_hidden_models};
use crate::filesystem::{
    FileTransaction, FilesystemError, VaultStore, atomic_write_config, with_file_transaction,
};
use crate::model_values::{
    input_modalities_known, normalize_input_modalities, normalize_output_modalities,
    normalize_reasoning_levels, normalize_supported_protocols, output_modalities_known,
    supported_protocols_known,
};
use emp_core::{deployment_identity, endpoint_fingerprint};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf, absolute};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CONFIG_PATH_ENV: &str = "EASY_MULTI_PROVIDER_CONFIG";
const MAX_CATALOG_ALIAS_BYTES: usize = 512;
const REASONING_SUMMARIES: [&str; 3] = ["auto", "show", "hide"];
const MASKED_API_KEY: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";
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
    python_type: &'static str,
}

impl ConfigError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            python_type: "ConfigError",
        }
    }

    fn python(python_type: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            python_type,
        }
    }

    /// Exception class produced by the Python compatibility oracle.
    pub fn python_type(&self) -> &'static str {
        self.python_type
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl From<FilesystemError> for ConfigError {
    fn from(error: FilesystemError) -> Self {
        Self::new(error.to_string())
    }
}

pub type ConfigResult<T> = Result<T, ConfigError>;

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
        Some(Value::String(value)) => python_capability_trim(value),
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
const TOP_LEVEL_PROVENANCE_FIELDS: [&str; 11] = [
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "supports_image_detail_original",
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

fn valid_iso_clock(value: &str) -> bool {
    let Some(base) = strip_fraction(value) else {
        return false;
    };
    let fields = if base.contains(':') {
        let fields = base.split(':').collect::<Vec<_>>();
        if !(1..=3).contains(&fields.len()) {
            return false;
        }
        fields
    } else {
        match base.len() {
            2 => vec![&base[..2]],
            4 => vec![&base[..2], &base[2..4]],
            6 => vec![&base[..2], &base[2..4], &base[4..6]],
            _ => return false,
        }
    };
    two_digit_number(fields[0], 23)
        && fields
            .get(1)
            .is_none_or(|minute| two_digit_number(minute, 59))
        && fields
            .get(2)
            .is_none_or(|second| two_digit_number(second, 59))
}

fn valid_iso_time(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let (time, offset) = if let Some(time) = value.strip_suffix('Z') {
        if time.contains('Z') {
            return false;
        }
        (time, None)
    } else if value.contains('Z') {
        return false;
    } else if let Some(index) = value
        .char_indices()
        .skip(1)
        .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))
    {
        (&value[..index], Some(&value[index + 1..]))
    } else {
        (value, None)
    };
    valid_iso_clock(time) && offset.is_none_or(valid_iso_clock)
}

fn weekday(year: u32, month: u32, day: u32) -> u32 {
    const MONTH_OFFSETS: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let adjusted_year = if month < 3 { year - 1 } else { year };
    (adjusted_year + adjusted_year / 4 - adjusted_year / 100
        + adjusted_year / 400
        + MONTH_OFFSETS[(month - 1) as usize]
        + day)
        % 7
}

fn valid_iso_week_date(value: &str) -> bool {
    let (year, week, day) = match value.len() {
        7 if value.as_bytes().get(4) == Some(&b'W') => (&value[..4], &value[5..7], "1"),
        8 if value.as_bytes().get(4) == Some(&b'W') => (&value[..4], &value[5..7], &value[7..8]),
        8 if value.get(4..6) == Some("-W") => (&value[..4], &value[6..8], "1"),
        10 if value.get(4..6) == Some("-W") && value.as_bytes().get(8) == Some(&b'-') => {
            (&value[..4], &value[6..8], &value[9..10])
        }
        _ => return false,
    };
    let Ok(year) = year.parse::<u32>() else {
        return false;
    };
    let Ok(week) = week.parse::<u32>() else {
        return false;
    };
    let Ok(day) = day.parse::<u32>() else {
        return false;
    };
    if year == 0 || !(1..=7).contains(&day) {
        return false;
    }
    let jan_first = weekday(year, 1, 1);
    let maximum_week = if jan_first == 4 || (jan_first == 3 && leap_year(year)) {
        53
    } else {
        52
    };
    (1..=maximum_week).contains(&week)
}

fn valid_basic_iso_date(value: &str) -> bool {
    value.len() == 8
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && valid_iso_date(&format!(
            "{}-{}-{}",
            &value[..4],
            &value[4..6],
            &value[6..8]
        ))
}

fn python_iso_date_prefix_length(value: &str) -> Option<usize> {
    for length in [10usize, 8, 7] {
        let Some(candidate) = value.get(..length) else {
            continue;
        };
        if valid_iso_date(candidate)
            || valid_basic_iso_date(candidate)
            || valid_iso_week_date(candidate)
        {
            return Some(length);
        }
    }
    None
}

fn valid_python_iso_timestamp(value: &str) -> bool {
    let Some(date_length) = python_iso_date_prefix_length(value) else {
        return false;
    };
    let Some(rest) = value.get(date_length..) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let mut characters = rest.chars();
    let Some(separator) = characters.next() else {
        return false;
    };
    if separator == 'Z' {
        return false;
    }
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

const CONTEXT_CALIBRATION_LIMIT: usize = 8;
const CONTEXT_CALIBRATION_PROTOCOLS: [&str; 3] =
    ["responses", "chat_completions", "anthropic_messages"];
const MAX_CONTEXT_WINDOW: i64 = 100_000_000;

fn unicode_decimal_digit(character: char) -> Option<u8> {
    const ZEROES: [u32; 76] = [
        0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
        0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
        0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
        0xff10, 0x104a0, 0x10d30, 0x10d40, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450,
        0x114d0, 0x11650, 0x116c0, 0x116d0, 0x116da, 0x11730, 0x118e0, 0x11950, 0x11bf0, 0x11c50,
        0x11d50, 0x11da0, 0x11f50, 0x16130, 0x16a60, 0x16ac0, 0x16b50, 0x16d70, 0x1ccf0, 0x1d7ce,
        0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e5f1, 0x1e950, 0x1fbf0,
    ];
    let codepoint = u32::from(character);
    ZEROES.iter().find_map(|zero| {
        let difference = codepoint.checked_sub(*zero)?;
        (difference < 10).then_some(difference as u8)
    })
}

fn python_int_string(raw: &str) -> Option<i64> {
    let text = python_capability_trim(raw);
    let (negative, digits) = if let Some(digits) = text.strip_prefix('-') {
        (true, digits)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let mut value = 0i64;
    let mut saw_digit = false;
    let mut previous_was_digit = false;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        let digit = i64::from(unicode_decimal_digit(character)?);
        value = value
            .saturating_mul(10)
            .saturating_add(digit)
            .min(MAX_CONTEXT_WINDOW + 1);
        saw_digit = true;
        previous_was_digit = true;
    }
    if !saw_digit || !previous_was_digit {
        return None;
    }
    Some(if negative { -value } else { value })
}

fn python_u64_string(raw: &str) -> Option<(bool, u64)> {
    let text = python_capability_trim(raw);
    let (negative, digits) = if let Some(digits) = text.strip_prefix('-') {
        (true, digits)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let mut value = 0u64;
    let mut saw_digit = false;
    let mut previous_was_digit = false;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        let digit = u64::from(unicode_decimal_digit(character)?);
        value = value.checked_mul(10)?.checked_add(digit)?;
        saw_digit = true;
        previous_was_digit = true;
    }
    (saw_digit && previous_was_digit).then_some((negative, value))
}

fn python_int(raw: &Value) -> Result<i64, ()> {
    match raw {
        Value::Bool(value) => Ok(i64::from(*value)),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(integer)
            } else if let Some(unsigned) = value.as_u64() {
                Ok(i64::try_from(unsigned).unwrap_or(MAX_CONTEXT_WINDOW + 1))
            } else {
                let number = value.as_f64().ok_or(())?;
                if !number.is_finite() {
                    return Err(());
                }
                let truncated = number.trunc();
                if truncated > MAX_CONTEXT_WINDOW as f64 {
                    Ok(MAX_CONTEXT_WINDOW + 1)
                } else if truncated < -(MAX_CONTEXT_WINDOW as f64) {
                    Ok(-(MAX_CONTEXT_WINDOW + 1))
                } else {
                    Ok(truncated as i64)
                }
            }
        }
        Value::String(value) => python_int_string(value).ok_or(()),
        _ => Err(()),
    }
}

fn python_int_conversion_error(raw: &Value) -> ConfigError {
    match raw {
        Value::String(value) => ConfigError::python(
            "ValueError",
            format!(
                "invalid literal for int() with base 10: '{}'",
                value
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t")
            ),
        ),
        Value::Array(_) => ConfigError::python(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'list'",
        ),
        Value::Object(_) => ConfigError::python(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'",
        ),
        Value::Null => ConfigError::python(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
        ),
        _ => ConfigError::python("ValueError", "invalid integer value"),
    }
}

fn model_python_int_or_zero(raw: Option<&Value>) -> ConfigResult<i64> {
    match raw {
        None => Ok(0),
        Some(value) if !json_truthy(value) => Ok(0),
        Some(value) => python_int(value).map_err(|_| python_int_conversion_error(value)),
    }
}

fn normalize_created_at(raw: Option<&Value>) -> ConfigResult<Value> {
    let Some(raw) = raw.filter(|value| json_truthy(value)) else {
        return Ok(Value::Number(0.into()));
    };
    let value = match raw {
        Value::Bool(true) => Value::Number(1.into()),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                if integer < 0 {
                    return Err(ConfigError::new("model.created_at cannot be negative"));
                }
                Value::Number(integer.into())
            } else if let Some(unsigned) = number.as_u64() {
                Value::Number(unsigned.into())
            } else {
                let number = number
                    .as_f64()
                    .ok_or_else(|| ConfigError::new("model.created_at must be numeric"))?;
                if !number.is_finite() || number > u64::MAX as f64 {
                    return Err(ConfigError::new("model.created_at is too large"));
                }
                let integer = number.trunc();
                if integer < 0.0 {
                    return Err(ConfigError::new("model.created_at cannot be negative"));
                }
                Value::Number((integer as u64).into())
            }
        }
        Value::String(value) => {
            let (negative, integer) =
                python_u64_string(value).ok_or_else(|| python_int_conversion_error(raw))?;
            if negative && integer != 0 {
                return Err(ConfigError::new("model.created_at cannot be negative"));
            }
            Value::Number(integer.into())
        }
        _ => return Err(python_int_conversion_error(raw)),
    };
    Ok(value)
}

fn python_float(raw: &Value) -> Result<f64, ()> {
    let value = match raw {
        Value::Bool(value) => f64::from(*value),
        Value::Number(value) => value.as_f64().ok_or(())?,
        Value::String(value) => {
            let text = python_capability_trim(value);
            let mut normalized = String::with_capacity(text.len());
            let mut previous_was_digit = false;
            let mut characters = text.chars().peekable();
            while let Some(character) = characters.next() {
                if character == '_' {
                    if !previous_was_digit
                        || characters
                            .peek()
                            .and_then(|next| unicode_decimal_digit(*next))
                            .is_none()
                    {
                        return Err(());
                    }
                    previous_was_digit = false;
                    continue;
                }
                if let Some(digit) = unicode_decimal_digit(character) {
                    normalized.push(char::from(b'0' + digit));
                    previous_was_digit = true;
                } else {
                    normalized.push(character);
                    previous_was_digit = false;
                }
            }
            let lowercase = normalized.to_ascii_lowercase();
            match lowercase.as_str() {
                "nan" | "+nan" | "-nan" => f64::NAN,
                "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
                "-inf" | "-infinity" => f64::NEG_INFINITY,
                _ => normalized.parse::<f64>().map_err(|_| ())?,
            }
        }
        _ => return Err(()),
    };
    Ok(value)
}

fn calibration_confidence(raw: &Value, name: &str) -> ConfigResult<Value> {
    let value = python_float(raw).map_err(|_| {
        ConfigError::new(format!("model.context_calibrations.{name} must be numeric"))
    })?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::new(format!(
            "model.context_calibrations.{name} is out of range"
        )));
    }
    Ok(Value::Number(
        serde_json::Number::from_f64(value).expect("finite confidence"),
    ))
}

/// Normalize bounded model context calibration records exactly like Python.
///
/// Unknown fields are discarded, only the first eight entries are retained, and
/// numeric estimates preserve Python's permissive integer parsing. The result
/// has the normalized caller shape consumed by `_normalize_model`.
pub fn normalize_context_calibrations(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Array(Vec::new())),
        Some(Value::Array(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(
                "model.context_calibrations must be a list",
            ));
        }
    };
    let mut result = Vec::new();
    for item in raw.iter().take(CONTEXT_CALIBRATION_LIMIT) {
        let Value::Object(item) = item else {
            return Err(ConfigError::new(
                "model.context_calibrations entries must be objects",
            ));
        };
        let fingerprint = string_value(
            item.get("endpoint_fingerprint"),
            "model.context_calibrations.endpoint_fingerprint",
            false,
        )?;
        let valid_fingerprint = fingerprint.len() == 71
            && fingerprint.starts_with("sha256:")
            && fingerprint[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if !valid_fingerprint {
            return Err(ConfigError::new(
                "model.context_calibrations endpoint fingerprint is invalid",
            ));
        }
        let protocol = string_value(
            item.get("protocol"),
            "model.context_calibrations.protocol",
            false,
        )?;
        if !CONTEXT_CALIBRATION_PROTOCOLS.contains(&protocol.as_str()) {
            return Err(ConfigError::new(
                "model.context_calibrations protocol is invalid",
            ));
        }
        let upstream = safe_capability_identity(
            item.get("upstream_model"),
            "model.context_calibrations.upstream_model",
        )?;
        let deployment = safe_capability_identity(
            item.get("deployment_identity"),
            "model.context_calibrations.deployment_identity",
        )?;
        if upstream.is_empty() || deployment.is_empty() {
            return Err(ConfigError::new(
                "model.context_calibrations identity is required",
            ));
        }
        let mut clean = Map::new();
        clean.insert(
            "endpoint_fingerprint".to_owned(),
            Value::String(fingerprint),
        );
        clean.insert("upstream_model".to_owned(), Value::String(upstream));
        clean.insert("protocol".to_owned(), Value::String(protocol));
        clean.insert("deployment_identity".to_owned(), Value::String(deployment));
        for (name, value) in [
            (
                "largest_success_estimate",
                item.get("largest_success_estimate"),
            ),
            (
                "smallest_failure_estimate",
                item.get("smallest_failure_estimate"),
            ),
        ] {
            let is_empty = value.is_none()
                || matches!(value, Some(Value::Null))
                || matches!(value, Some(Value::String(value)) if value.is_empty());
            let normalized = if is_empty {
                Value::Null
            } else {
                let integer = python_int(value.expect("non-empty estimate")).map_err(|_| {
                    ConfigError::new(format!("model.context_calibrations.{name} must be numeric"))
                })?;
                if integer <= 0 || integer > MAX_CONTEXT_WINDOW {
                    return Err(ConfigError::new(format!(
                        "model.context_calibrations.{name} is out of range"
                    )));
                }
                Value::Number(serde_json::Number::from(integer))
            };
            clean.insert(name.to_owned(), normalized);
        }
        for name in ["largest_success_source", "smallest_failure_source"] {
            let source = match item.get(name) {
                None => "unknown".to_owned(),
                raw => string_value(raw, &format!("model.context_calibrations.{name}"), false)?,
            };
            if source != "observed" && source != "unknown" {
                return Err(ConfigError::new(format!(
                    "model.context_calibrations.{name} is invalid"
                )));
            }
            clean.insert(name.to_owned(), Value::String(source));
        }
        for name in ["largest_success_confidence", "smallest_failure_confidence"] {
            let estimate_name = if name == "largest_success_confidence" {
                "largest_success_estimate"
            } else {
                "smallest_failure_estimate"
            };
            let default = if clean
                .get(estimate_name)
                .is_some_and(|value| !value.is_null())
            {
                1.0
            } else {
                0.0
            };
            let default_confidence =
                Value::Number(serde_json::Number::from_f64(default).expect("default confidence"));
            let raw_confidence = item.get(name).unwrap_or(&default_confidence);
            clean.insert(
                name.to_owned(),
                calibration_confidence(raw_confidence, name)?,
            );
        }
        for name in [
            "largest_success_observed_at",
            "smallest_failure_observed_at",
        ] {
            let observed_at = match item.get(name) {
                None | Some(Value::Null) => Value::Null,
                Some(Value::String(value)) if valid_python_iso_timestamp(value) => {
                    Value::String(value.clone())
                }
                Some(_) => {
                    return Err(ConfigError::new(format!(
                        "model.context_calibrations.{name} is invalid"
                    )));
                }
            };
            clean.insert(name.to_owned(), observed_at);
        }
        result.push(Value::Object(clean));
    }
    Ok(Value::Array(result))
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

fn strict_reasoning_levels(raw: Option<&Value>) -> ConfigResult<Vec<String>> {
    let default = Value::Array(Vec::new());
    let raw = raw.unwrap_or(&default);
    let Value::Array(items) = raw else {
        return Err(ConfigError::new("model.reasoning_levels must be a list"));
    };
    let mut levels = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(level) = item else {
            return Err(ConfigError::new("model.reasoning_levels must be a string"));
        };
        levels.push(python_capability_trim(level).to_owned());
    }
    if levels.iter().any(|level| level.is_empty()) {
        return Err(ConfigError::new(
            "model.reasoning_levels entries must not be empty",
        ));
    }
    Ok(normalize_reasoning_levels(Some(&Value::Array(
        levels.into_iter().map(Value::String).collect(),
    ))))
}

fn broad_model_id(raw: Option<&Value>, field: &str) -> ConfigResult<String> {
    let value = string_value(raw, field, true)?;
    let valid = !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | ':' | '-')
        });
    if !valid {
        return Err(ConfigError::new(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(value)
}

fn normalized_capability_object(raw: Option<&Value>, field: &str) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Object(Map::new())),
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(format!("{field} must be an object")));
        }
    };
    let mut normalized = Map::new();
    for name in MODEL_BOOLEAN_CAPABILITIES {
        let Some(value) = raw.get(name) else {
            continue;
        };
        let Value::Bool(value) = value else {
            return Err(ConfigError::new(format!("{field}.{name} must be boolean")));
        };
        normalized.insert((*name).to_owned(), Value::Bool(*value));
    }
    Ok(Value::Object(normalized))
}

/// Normalize one complete model record exactly like Python `_normalize_model`.
///
/// The result always has the 26 configured output fields; unknown model fields
/// are intentionally discarded. Capability-source normalization sees the
/// union of raw top-level and nested capability keys, but uses the normalized
/// record for effective capability values.
pub fn normalize_model(raw: &Value) -> ConfigResult<Value> {
    let Value::Object(raw) = raw else {
        return Err(ConfigError::new("each model must be an object"));
    };
    let levels = strict_reasoning_levels(raw.get("reasoning_levels"))?;
    let raw_reasoning_support = raw.get("supports_reasoning");
    let supports_reasoning = match raw_reasoning_support {
        None | Some(Value::Null) => {
            if levels.is_empty() {
                Value::Null
            } else {
                Value::Bool(true)
            }
        }
        Some(Value::Bool(value)) => Value::Bool(*value),
        Some(_) => {
            return Err(ConfigError::new(
                "model.supports_reasoning must be boolean or null",
            ));
        }
    };
    let supports_reasoning_summaries = match raw.get("supports_reasoning_summaries") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Bool(value)) => Value::Bool(*value),
        Some(_) => {
            return Err(ConfigError::new(
                "model.supports_reasoning_summaries must be boolean or null",
            ));
        }
    };
    let context_window = model_python_int_or_zero(raw.get("context_window"))?;
    if context_window < 0 {
        return Err(ConfigError::new("model.context_window cannot be negative"));
    }
    if context_window > MAX_CONTEXT_WINDOW {
        return Err(ConfigError::new("model.context_window is too large"));
    }
    let output_raw = if raw.contains_key("output_limit") {
        raw.get("output_limit")
    } else {
        raw.get("output_token_limit")
    };
    let output_limit = model_python_int_or_zero(output_raw)?;
    if output_limit < 0 {
        return Err(ConfigError::new("model.output_limit cannot be negative"));
    }
    if output_limit > MAX_CONTEXT_WINDOW {
        return Err(ConfigError::new("model.output_limit is too large"));
    }
    let created_at = normalize_created_at(raw.get("created_at"))?;
    let visibility = string_value(raw.get("visibility"), "model.visibility", false)?;
    let visibility = if visibility.is_empty() {
        "list".to_owned()
    } else if visibility == "list" || visibility == "hide" {
        visibility
    } else {
        return Err(ConfigError::new("model.visibility must be list or hide"));
    };
    let supports_image_detail_original = match raw.get("supports_image_detail_original") {
        Some(Value::Bool(value)) => *value,
        _ => false,
    };
    let max_input_tokens = model_python_int_or_zero(raw.get("max_input_tokens"))?;
    if max_input_tokens < 0 {
        return Err(ConfigError::new(
            "model.max_input_tokens cannot be negative",
        ));
    }
    if max_input_tokens > MAX_CONTEXT_WINDOW {
        return Err(ConfigError::new("model.max_input_tokens is too large"));
    }
    let reasoning_control = string_value(raw.get("reasoning_control"), "value", false)?;
    let output_modalities = normalize_output_modalities(raw.get("output_modalities"));
    let supported_protocols = normalize_supported_protocols(raw.get("supported_protocols"));
    let mut model = serde_json::json!({
        "id": broad_model_id(raw.get("id"), "model.id")?,
        "provider": broad_model_id(raw.get("provider"), "model.provider")?,
        "upstream_id": string_value(raw.get("upstream_id"), "value", false)?,
        "family_id": safe_capability_identity(
            raw.get("family_id"),
            "model.family_id",
        )?,
        "display_name": string_value(raw.get("display_name"), "value", false)?,
        "description": string_value(raw.get("description"), "value", false)?,
        "supports_reasoning": supports_reasoning,
        "supports_reasoning_summaries": supports_reasoning_summaries,
        "reasoning_levels": levels,
        "reasoning_control": reasoning_control,
        "context_window": context_window,
        "max_input_tokens": max_input_tokens,
        "output_limit": output_limit,
        "created_at": created_at,
        "enabled": raw.get("enabled").is_none_or(json_truthy),
        "visibility": visibility,
        "input_modalities": normalize_input_modalities(raw.get("input_modalities")),
        "output_modalities": output_modalities,
        "supported_protocols": supported_protocols,
        "supports_image_detail_original": supports_image_detail_original,
        "deployment_identity": safe_capability_identity(
            raw.get("deployment_identity"),
            "model.deployment_identity",
        )?,
        "resolved_protocol": normalize_resolved_protocol(
            raw.get("resolved_protocol"),
        )?,
        "protocol_observation": normalize_protocol_observation(
            raw.get("protocol_observation"),
        )?,
        "context_calibrations": normalize_context_calibrations(
            raw.get("context_calibrations"),
        )?,
        "capabilities": normalized_capability_object(
            raw.get("capabilities"),
            "model.capabilities",
        )?,
    });
    let mut explicit_fields = raw.keys().map(String::as_str).collect::<Vec<_>>();
    if let Some(Value::Object(capabilities)) = raw.get("capabilities") {
        explicit_fields.extend(capabilities.keys().map(String::as_str));
    }
    model["capability_sources"] = normalize_model_capability_sources(
        raw.get("capability_sources"),
        &model,
        Some(&explicit_fields),
    )?;
    Ok(model)
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

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: i64 = 4200;
const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

fn default_native_catalog_path() -> String {
    let home = if cfg!(windows) {
        env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))
    } else {
        env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))
    };
    let mut path = home.map_or_else(PathBuf::new, PathBuf::from);
    path.push(".codex");
    path.push("models_cache.json");
    path.to_string_lossy().into_owned()
}

fn default_configuration() -> Value {
    serde_json::json!({
        "host": DEFAULT_HOST,
        "port": DEFAULT_PORT,
        "native_catalog_path": default_native_catalog_path(),
        "account_store_path": "state/accounts",
        "secret_store_path": "state/secrets",
        "codex_base_url": DEFAULT_CODEX_BASE_URL,
        "accounts": [],
        "providers": [],
        "models": [],
        "native_hidden_models": [],
        "native_model_context_windows": {},
        "catalog_presentations": {},
        "catalog_family_presentations": {},
        "subscription_search": {"enabled": false, "account_id": ""},
        "codex_runtime_sources": ["auto"],
    })
}

fn configuration_string(raw: Option<&Value>, default: &str, field: &str) -> ConfigResult<String> {
    match raw {
        None => Ok(default.to_owned()),
        value => string_value(value, field, false),
    }
}

fn validate_codex_base_url(raw: Option<&Value>) -> ConfigResult<String> {
    let value = configuration_string(raw, DEFAULT_CODEX_BASE_URL, "codex_base_url")?;
    if value.is_empty() {
        return Err(ConfigError::new("codex_base_url is required"));
    }
    let value = value.trim_end_matches('/');
    let parsed = parse_provider_url(value)
        .ok_or_else(|| ConfigError::new("codex_base_url must be an http(s) URL"))?;
    if !matches!(parsed.scheme.as_str(), "http" | "https") || parsed.netloc.is_empty() {
        return Err(ConfigError::new("codex_base_url must be an http(s) URL"));
    }
    let (username, password) = userinfo(parsed.netloc);
    if !username.is_empty() || password.is_some_and(|password| !password.is_empty()) {
        return Err(ConfigError::new(
            "codex_base_url must not contain URL credentials",
        ));
    }
    if !parsed.query.is_empty() || !parsed.fragment.is_empty() {
        return Err(ConfigError::new(
            "codex_base_url must not contain a query or fragment",
        ));
    }
    let host = hostname(parsed.netloc).unwrap_or("").to_ascii_lowercase();
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if parsed.scheme == "http" && !loopback {
        return Err(ConfigError::new(
            "codex_base_url must use HTTPS unless it targets loopback",
        ));
    }
    Ok(value.to_owned())
}

fn python_iterable(raw: Option<&Value>) -> ConfigResult<Vec<Value>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    match raw {
        Value::Array(values) => Ok(values.clone()),
        Value::String(value) => Ok(value
            .chars()
            .map(|character| Value::String(character.to_string()))
            .collect()),
        Value::Object(values) => Ok(values.keys().cloned().map(Value::String).collect()),
        Value::Null => Err(ConfigError::python(
            "TypeError",
            "'NoneType' object is not iterable",
        )),
        Value::Bool(_) => Err(ConfigError::python(
            "TypeError",
            "'bool' object is not iterable",
        )),
        Value::Number(value) if value.is_i64() || value.is_u64() => Err(ConfigError::python(
            "TypeError",
            "'int' object is not iterable",
        )),
        Value::Number(_) => Err(ConfigError::python(
            "TypeError",
            "'float' object is not iterable",
        )),
    }
}

fn account_error(error: impl fmt::Display) -> ConfigError {
    ConfigError::python("AccountError", error.to_string())
}

pub(crate) fn path_python_resolve(path: &Path) -> PathBuf {
    fn recurse(path: &Path, followed_links: usize) -> PathBuf {
        let absolute = absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let mut resolved = PathBuf::new();
        let components = absolute.components().collect::<Vec<_>>();
        for (index, component) in components.iter().enumerate() {
            use std::path::Component;
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Prefix(_) | Component::RootDir => resolved.push(component.as_os_str()),
                Component::Normal(name) => {
                    let candidate = resolved.join(name);
                    let is_link = std::fs::symlink_metadata(&candidate)
                        .is_ok_and(|metadata| metadata.file_type().is_symlink());
                    if is_link
                        && followed_links < 64
                        && let Ok(target) = std::fs::read_link(&candidate)
                    {
                        let mut redirected = if target.is_absolute() {
                            target
                        } else {
                            resolved.join(target)
                        };
                        for remaining in &components[index + 1..] {
                            redirected.push(remaining.as_os_str());
                        }
                        return recurse(&redirected, followed_links + 1);
                    }
                    resolved.push(name);
                }
            }
        }
        resolved
    }
    recurse(path, 0)
}

pub(crate) fn account_root(config: &Value) -> PathBuf {
    expand_user(Path::new(
        config
            .get("account_store_path")
            .and_then(Value::as_str)
            .unwrap_or("state/accounts"),
    ))
}

/// Expand `~` exactly for paths escaped from the configuration crate.
pub(crate) fn expand_home(path: &Path) -> PathBuf {
    expand_user(path)
}

/// Return the Python-native catalog fallback, expanded in the caller's home.
pub(crate) fn expand_home_default_native_catalog_path() -> PathBuf {
    expand_user(Path::new(&default_native_catalog_path()))
}

fn expand_user(path: &Path) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    else {
        return path.to_path_buf();
    };
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map_or_else(|| path.to_path_buf(), |home| PathBuf::from(home).join(rest))
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        let character = byte as char;
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-' | '~') {
            encoded.push(character);
        } else {
            encoded.push('%');
            encoded.push_str(format!("{byte:02X}").as_str());
        }
    }
    encoded
}

fn canonical_account_paths(config: &mut Value, config_path: &Path) -> Result<(), ConfigError> {
    let raw_accounts = config
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or_else(|| ConfigError::new("accounts must be a list"))?;
    let entries = raw_accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            (
                index,
                account
                    .get("auth_file")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                account.get("id").and_then(Value::as_str).map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    for (index, raw_path, id) in entries {
        if raw_path.is_empty() {
            continue;
        }
        let Some(id) = id else {
            return Err(ConfigError::new(
                "account.id must be a safe single path segment",
            ));
        };
        let expected = crate::accounts::account_auth_path(config, &id, config_path)
            .map_err(|error| ConfigError::new(error.to_string()))?;
        let mut actual = expand_user(Path::new(&raw_path));
        if !actual.is_absolute() {
            actual = config_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(actual);
        }
        let actual_is_symlink = std::fs::symlink_metadata(&actual)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        let actual = path_python_resolve(&actual);
        if actual != expected || actual_is_symlink {
            return Err(ConfigError::new(
                "account.auth_file must be managed inside the account store",
            ));
        }
        let account = config
            .get_mut("accounts")
            .and_then(Value::as_array_mut)
            .and_then(|accounts| accounts.get_mut(index))
            .ok_or_else(|| ConfigError::new("accounts must be a list"))?;
        if let Some(object) = account.as_object_mut() {
            object.insert(
                "auth_file".to_owned(),
                Value::from(expected.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(())
}

/// Canonicalize configured account credentials against the derived account root.
///
/// This is the public Rust compatibility slice for Python
/// `canonicalize_account_paths`. Account path errors are represented as
/// `ConfigError` because Python's `_canonicalize_private_paths` catches the
/// original `AccountError` and re-raises it as a `ConfigError`.
pub fn canonicalize_account_paths(config: &mut Value, config_path: &Path) -> ConfigResult<()> {
    canonical_account_paths(config, config_path)
}

fn canonical_secret_root(config: &Value, config_path: &Path) -> PathBuf {
    let mut root = expand_user(Path::new(
        config
            .get("secret_store_path")
            .and_then(Value::as_str)
            .unwrap_or("state/secrets"),
    ));
    if !root.is_absolute() {
        root = config_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(root);
    }
    path_python_resolve(&root)
}

fn canonical_secret_paths(config: &mut Value, config_path: &Path) -> Result<(), ConfigError> {
    let base = config_path
        .parent()
        .ok_or_else(|| ConfigError::new("configuration path must have a parent"))?;
    let mut root = expand_user(Path::new(
        config
            .get("secret_store_path")
            .and_then(Value::as_str)
            .unwrap_or("state/secrets"),
    ));
    if !root.is_absolute() {
        root = base.join(root);
    }
    let root = path_python_resolve(&root);
    let providers = config
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| ConfigError::new("providers must be a list"))?;
    for provider in providers {
        let raw_path = provider
            .get("api_key_file")
            .and_then(Value::as_str)
            .unwrap_or("");
        if raw_path.is_empty() {
            continue;
        }
        let Some(id) = provider.get("id").and_then(Value::as_str) else {
            return Err(ConfigError::new(
                "provider.id must be a safe single path segment",
            ));
        };
        let expected = root.join(percent_encode(id) + ".key.enc");
        let actual = expand_user(Path::new(raw_path));
        let actual = if actual.is_absolute() {
            actual
        } else {
            config_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(actual)
        };
        let actual_is_symlink = std::fs::symlink_metadata(&actual)
            .is_ok_and(|metadata| metadata.file_type().is_symlink());
        let actual = path_python_resolve(&actual);
        if actual != expected || actual_is_symlink {
            return Err(ConfigError::new(
                "provider.api_key_file must be managed inside the secret store",
            ));
        }
        if let Some(object) = provider.as_object_mut() {
            object.insert(
                "api_key_file".to_owned(),
                Value::String(expected.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(())
}

/// Canonicalize managed private paths for already normalized configuration.
///
/// This is the public Rust compatibility slice for Python
/// `_canonicalize_private_paths`. Relative store paths and relative managed
/// files are based on `config_path.parent`, `~` is expanded, missing path
/// components are retained, existing links are followed, and the final managed
/// input path must itself not be a symlink.
pub fn canonicalize_private_paths(config: &mut Value, config_path: &Path) -> ConfigResult<()> {
    canonical_account_paths(config, config_path)?;
    canonical_secret_paths(config, config_path)
}

fn utc_date_parts(seconds: u64) -> (i64, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

pub fn observed_at_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs();
    let microseconds = elapsed.subsec_micros();
    let (year, month, day) = utc_date_parts(seconds);
    let second_of_day = seconds % 86_400;
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    if microseconds == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{microseconds:06}+00:00"
        )
    }
}

/// Return a normalized configuration with one successful automatic protocol
/// observation applied. Explicit providers and missing identities are left
/// unchanged, matching the Python server's request-completion callback.
pub fn remember_resolved_protocol_at(
    config: &Value,
    provider_id: &str,
    requested_model: &str,
    protocol: &str,
    observed_at: &str,
) -> ConfigResult<Option<Value>> {
    if !matches!(
        protocol,
        "responses" | "chat_completions" | "anthropic_messages"
    ) {
        return Ok(None);
    }
    let mut updated = config.clone();
    let Some(root) = updated.as_object_mut() else {
        return Ok(None);
    };
    let Some(providers) = root.get("providers").and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(provider_index) = providers.iter().position(|provider| {
        provider.get("id").and_then(Value::as_str) == Some(provider_id)
            && provider.get("protocol").and_then(Value::as_str) == Some("auto")
    }) else {
        return Ok(None);
    };

    let model_index = root
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().position(|model| {
                model.get("id").and_then(Value::as_str) == Some(requested_model)
                    && model.get("provider").and_then(Value::as_str) == Some(provider_id)
            })
        });
    let provider = providers[provider_index]
        .as_object()
        .expect("normalized providers are objects")
        .clone();
    let model = model_index
        .and_then(|index| root.get("models")?.as_array()?.get(index))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let upstream_model = model
        .get("upstream_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(requested_model);
    let observation = serde_json::json!({
        "source": "observed",
        "confidence": 1.0,
        "observed_at": observed_at,
        "endpoint_fingerprint": endpoint_fingerprint(
            provider.get("base_url").and_then(Value::as_str)
        ),
        "deployment_identity": deployment_identity(&provider, &model),
        "upstream_model": upstream_model,
    });

    let provider = root
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .and_then(|providers| providers.get_mut(provider_index))
        .and_then(Value::as_object_mut)
        .expect("normalized providers are objects");
    provider.insert(
        "resolved_protocol".to_owned(),
        Value::String(protocol.to_owned()),
    );
    provider.insert("protocol_observation".to_owned(), observation.clone());
    if let Some(index) = model_index
        && let Some(model) = root
            .get_mut("models")
            .and_then(Value::as_array_mut)
            .and_then(|models| models.get_mut(index))
            .and_then(Value::as_object_mut)
    {
        model.insert(
            "resolved_protocol".to_owned(),
            Value::String(protocol.to_owned()),
        );
        model.insert("protocol_observation".to_owned(), observation);
    }
    normalize_configuration(Some(&updated)).map(Some)
}

pub fn remember_resolved_protocol(
    config: &Value,
    provider_id: &str,
    requested_model: &str,
    protocol: &str,
) -> ConfigResult<Option<Value>> {
    remember_resolved_protocol_at(
        config,
        provider_id,
        requested_model,
        protocol,
        &observed_at_now(),
    )
}

fn values_by_id(raw: Option<&Value>) -> Map<String, Value> {
    raw.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            Some((id.to_owned(), item.clone()))
        })
        .collect()
}

fn manual_provenance(observed_at: String) -> Value {
    serde_json::json!({
        "source": "manual",
        "confidence": 1.0,
        "observed_at": observed_at,
    })
}

fn unknown_provenance() -> Value {
    serde_json::json!({
        "source": "unknown",
        "confidence": 0.0,
        "observed_at": Value::Null,
    })
}

fn merge_web_update_at_time(
    current: &Value,
    incoming: &Value,
    mut observed_at: impl FnMut() -> String,
) -> ConfigResult<Value> {
    let Value::Object(_) = incoming else {
        return Err(ConfigError::new("request body must be an object"));
    };
    let mut merged = incoming.clone();
    let merged_object = merged.as_object_mut().expect("checked object");

    merged_object.insert(
        "codex_runtime_sources".to_owned(),
        current
            .get("codex_runtime_sources")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![Value::String("auto".to_owned())])),
    );
    for field in [
        "account_store_path",
        "secret_store_path",
        "native_catalog_path",
    ] {
        if let Some(value) = current.get(field) {
            merged_object.insert(field.to_owned(), value.clone());
        }
    }

    let old_providers = values_by_id(current.get("providers"));
    if let Some(Value::Array(providers)) = merged_object.get_mut("providers") {
        for provider_value in providers {
            let Some(provider) = provider_value.as_object_mut() else {
                continue;
            };
            let old = provider
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| old_providers.get(id));
            let masked = provider
                .get("api_key")
                .is_some_and(|value| value == "••••••••");
            if !provider.contains_key("api_key") || masked {
                provider.insert(
                    "api_key".to_owned(),
                    old.and_then(|value| value.get("api_key"))
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new())),
                );
            }
            let incoming_file = provider.get("api_key_file");
            let old_file = old.and_then(|value| value.get("api_key_file"));
            if incoming_file.is_some_and(json_truthy) && incoming_file != old_file {
                return Err(ConfigError::new("provider.api_key_file is managed by EMP"));
            }
            if old_file.is_some_and(json_truthy) {
                provider.insert(
                    "api_key_file".to_owned(),
                    old_file.expect("truthy old file").clone(),
                );
            }
            if old.is_some_and(|old| {
                ["base_url", "protocol", "deployment_identity"]
                    .iter()
                    .any(|field| provider.get(*field) != old.get(*field))
            }) {
                provider.insert("resolved_protocol".to_owned(), Value::String(String::new()));
                provider.insert("protocol_observation".to_owned(), Value::Object(Map::new()));
            }
        }
    }

    let old_accounts = values_by_id(current.get("accounts"));
    if let Some(Value::Array(accounts)) = merged_object.get_mut("accounts") {
        for account_value in accounts {
            let Some(account) = account_value.as_object_mut() else {
                continue;
            };
            let old = account
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| old_accounts.get(id));
            let incoming_file = account.get("auth_file");
            let old_file = old.and_then(|value| value.get("auth_file"));
            if incoming_file.is_some_and(json_truthy) && incoming_file != old_file {
                return Err(ConfigError::new("account.auth_file is managed by EMP"));
            }
            if old_file.is_some_and(json_truthy) {
                account.insert(
                    "auth_file".to_owned(),
                    old_file.expect("truthy old file").clone(),
                );
            }
        }
    }

    let old_models = values_by_id(current.get("models"));
    if let Some(Value::Array(models)) = merged_object.get_mut("models") {
        for model_value in models {
            let Some(model) = model_value.as_object_mut() else {
                continue;
            };
            let old = model
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| old_models.get(id));
            model.insert(
                "context_calibrations".to_owned(),
                old.and_then(|value| value.get("context_calibrations"))
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
            );
            if let Some(old) = old {
                for field in [
                    "visibility",
                    "supports_reasoning",
                    "supports_reasoning_summaries",
                    "input_modalities",
                    "output_modalities",
                    "supported_protocols",
                    "reasoning_control",
                    "max_input_tokens",
                    "output_limit",
                    "supports_image_detail_original",
                    "deployment_identity",
                    "resolved_protocol",
                    "protocol_observation",
                ] {
                    if !model.contains_key(field) {
                        model.insert(
                            field.to_owned(),
                            old.get(field).cloned().unwrap_or(Value::Null),
                        );
                    }
                }
            }

            let old_capabilities = old
                .and_then(|value| value.get("capabilities"))
                .filter(|value| json_truthy(value));
            match (old_capabilities, model.get_mut("capabilities")) {
                (Some(old_capabilities), Some(Value::Object(incoming_capabilities))) => {
                    for field in MODEL_BOOLEAN_CAPABILITIES {
                        if !incoming_capabilities.contains_key(field)
                            && let Some(value) = old_capabilities.get(field)
                        {
                            incoming_capabilities.insert(field.to_owned(), value.clone());
                        }
                    }
                }
                (Some(old_capabilities), _) => {
                    model.insert("capabilities".to_owned(), old_capabilities.clone());
                }
                (None, _) => {}
            }

            let mut sources = old
                .and_then(|value| value.get("capability_sources"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let incoming_sources = model
                .get("capability_sources")
                .and_then(Value::as_object)
                .cloned();
            if let Some(incoming_sources) = &incoming_sources {
                sources.extend(incoming_sources.clone());
            }
            for field in TOP_LEVEL_PROVENANCE_FIELDS {
                if !model.contains_key(field) {
                    continue;
                }
                let changed = old.is_none_or(|old| model.get(field) != old.get(field));
                if !changed {
                    continue;
                }
                let requested_source = incoming_sources
                    .as_ref()
                    .and_then(|values| values.get(field))
                    .and_then(Value::as_object)
                    .and_then(|value| value.get("source"))
                    .and_then(Value::as_str);
                let provenance = if field == "input_modalities"
                    && requested_source == Some("unknown")
                    && !normalize_input_modalities(model.get(field))
                        .iter()
                        .any(|modality| modality == "image")
                {
                    unknown_provenance()
                } else {
                    manual_provenance(observed_at())
                };
                sources.insert(field.to_owned(), provenance);
            }

            let old_capabilities = old
                .and_then(|value| value.get("capabilities"))
                .and_then(Value::as_object);
            if let Some(capabilities) = model.get("capabilities").and_then(Value::as_object) {
                for field in MODEL_BOOLEAN_CAPABILITIES {
                    let Some(value) = capabilities.get(field) else {
                        continue;
                    };
                    if old.is_none()
                        || old_capabilities.and_then(|values| values.get(field)) != Some(value)
                    {
                        sources.insert(field.to_owned(), manual_provenance(observed_at()));
                    }
                }
            }
            if !sources.is_empty() {
                model.insert("capability_sources".to_owned(), Value::Object(sources));
            }
            if old.is_some_and(|old| {
                ["provider", "upstream_id", "deployment_identity"]
                    .iter()
                    .any(|field| model.get(*field) != old.get(*field))
            }) {
                model.insert("resolved_protocol".to_owned(), Value::String(String::new()));
                model.insert("protocol_observation".to_owned(), Value::Object(Map::new()));
            }
        }
    }
    normalize_configuration(Some(&merged))
}

/// Return the configured configuration path without environment side effects.
///
/// This mirrors Python's `config_path`: an absent environment variable selects
/// `config.json`; an explicitly empty value keeps current-directory semantics.
pub fn config_path() -> PathBuf {
    config_path_from_env(std::env::var_os(CONFIG_PATH_ENV).as_deref())
}

/// Resolve the configuration path used by [`load_configuration`].
fn config_path_from_env(value: Option<&std::ffi::OsStr>) -> PathBuf {
    match value {
        Some(value) => PathBuf::from(value),
        None => PathBuf::from("config.json"),
    }
}

/// Save a normalized configuration and its derived secret files atomically.
pub fn save_configuration(
    config: &Value,
    path: Option<&Path>,
    vault: &VaultStore,
) -> ConfigResult<PathBuf> {
    with_file_transaction(|transaction| {
        save_configuration_in_transaction(config, path, vault, transaction)
    })
}

/// Add a configuration save to a caller-owned transaction.
///
/// This is used by larger state changes such as migration imports. The caller
/// owns commit and rollback; this function never ends the transaction early.
pub fn save_configuration_in_transaction(
    config: &Value,
    path: Option<&Path>,
    vault: &VaultStore,
    transaction: &mut FileTransaction,
) -> ConfigResult<PathBuf> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => config_path(),
    };

    let mut previous_secret_files = Vec::new();
    if path.exists()
        && let Ok(previous) = load_configuration(Some(&path))
    {
        let mut values = previous
            .get("providers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|provider| {
                provider
                    .get("api_key_file")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        previous_secret_files = values;
    }

    let mut config = normalize_configuration(Some(config))?;
    canonicalize_private_paths(&mut config, &path)?;
    let secret_root = canonical_secret_root(&config, &path);
    if let Some(Value::Array(providers)) = config.get_mut("providers") {
        for provider in providers {
            let Some(object) = provider.as_object_mut() else {
                return Err(ConfigError::new("each provider must be an object"));
            };
            let api_key = object
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if api_key.is_empty() || api_key == MASKED_API_KEY {
                continue;
            }
            let Some(id) = object.get("id").and_then(Value::as_str) else {
                return Err(ConfigError::new("provider.id must be a string"));
            };
            let secret_path = secret_root.join(format!("{}.key.enc", percent_encode(id)));
            transaction.remember(&secret_path)?;
            vault.write_encrypted_text(&secret_path, &api_key)?;
            object.insert("api_key".to_owned(), Value::String(String::new()));
            object.insert(
                "api_key_file".to_owned(),
                Value::String(secret_path.to_string_lossy().into_owned()),
            );
        }
    }

    transaction.remember(&path)?;
    let serialized = serde_json::to_vec_pretty(&config)
        .map_err(|_| ConfigError::new("configuration could not be serialized"))?;
    let mut serialized = serialized;
    serialized.push(b'\n');
    atomic_write_config(&path, &serialized)?;

    let current_secret_files = config
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|provider| {
            provider
                .get("api_key_file")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    for obsolete in previous_secret_files
        .into_iter()
        .filter(|value| !current_secret_files.contains(value))
    {
        let obsolete_path = PathBuf::from(&obsolete);
        transaction.remember(&obsolete_path)?;
        match fs::remove_file(&obsolete_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // Python deliberately treats obsolete-secret cleanup as best effort.
            Err(_) => {}
        }
    }
    Ok(path)
}

/// Load, normalize and canonicalize the private paths in a configuration file.
///
/// This is Python's `config.load`. Missing files return the default
/// configuration. A successful UTF-8 JSON decode is passed through full
/// configuration normalization, then through managed-path canonicalization.
///
/// Python preserves the native `OSError` for non-JSON I/O failures while Rust
/// has no exception hierarchy, so this boundary records the corresponding
/// Python-visible error class while keeping a stable message. JSON syntax
/// errors remain `ConfigError`; parser-specific diagnostic wording may differ.
pub fn load_configuration(path: Option<&Path>) -> ConfigResult<Value> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => config_path(),
    };
    if !path.exists() {
        return normalize_configuration(None);
    }

    let raw = std::fs::read(&path).map_err(|error| {
        let python_type = match error.kind() {
            std::io::ErrorKind::NotFound => "FileNotFoundError",
            std::io::ErrorKind::PermissionDenied => "PermissionError",
            std::io::ErrorKind::IsADirectory => "IsADirectoryError",
            _ => "OSError",
        };
        ConfigError::python(python_type, "configuration file could not be read")
    })?;
    let raw = String::from_utf8(raw).map_err(|_| {
        ConfigError::python(
            "UnicodeDecodeError",
            "configuration file is not valid UTF-8",
        )
    })?;
    let value = serde_json::from_str::<Value>(&raw).map_err(|error| {
        ConfigError::new(format!("invalid JSON in {}: {}", path.display(), error))
    })?;
    let mut config = normalize_configuration(Some(&value))?;
    canonicalize_private_paths(&mut config, &path)?;
    Ok(config)
}

/// Resolve one request-local provider credential from normalized state.
///
/// Inline values take precedence. Managed encrypted files fail closed to an
/// empty value, matching Python's `config.api_key` boundary without exposing a
/// decryption or filesystem diagnostic to callers.
pub fn provider_api_key(provider: &Value, vault: &VaultStore) -> String {
    let Some(provider) = provider.as_object() else {
        return String::new();
    };
    if let Some(value) = provider
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return value.to_owned();
    }
    let Some(path) = provider
        .get("api_key_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(Path::new)
    else {
        return String::new();
    };
    if path.is_symlink() {
        return String::new();
    }
    vault
        .read_encrypted_text(path)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// Apply a Web update while preserving secrets and managed discovery metadata.
///
/// This is Python `merge_web_update` with its optional filesystem path omitted;
/// canonical private-path validation remains a separate state transition.
pub fn merge_web_update(current: &Value, incoming: &Value) -> ConfigResult<Value> {
    merge_web_update_at_time(current, incoming, observed_at_now)
}

/// Apply the same merge with a fixed timestamp for deterministic compatibility tests.
pub fn merge_web_update_with_time(
    current: &Value,
    incoming: &Value,
    observed_at: &str,
) -> ConfigResult<Value> {
    merge_web_update_at_time(current, incoming, || observed_at.to_owned())
}

/// Normalize the full top-level configuration without filesystem path canonicalization.
///
/// This is the pure Python `config.normalize` contract. Loading, saving, secret
/// extraction, and canonical private paths are separate state transitions.
pub fn normalize_configuration(raw: Option<&Value>) -> ConfigResult<Value> {
    if raw.is_none() || raw.is_some_and(Value::is_null) {
        return Ok(default_configuration());
    }
    let Some(Value::Object(raw)) = raw else {
        return Err(ConfigError::new("configuration must be an object"));
    };

    let host = configuration_string(raw.get("host"), DEFAULT_HOST, "value")?;
    let host = if host.is_empty() {
        DEFAULT_HOST.to_owned()
    } else {
        host
    };
    if host != DEFAULT_HOST {
        return Err(ConfigError::new(
            "host must be 127.0.0.1 for local-only management",
        ));
    }
    let port_raw = raw
        .get("port")
        .cloned()
        .unwrap_or_else(|| Value::from(DEFAULT_PORT));
    let port = python_int(&port_raw).map_err(|_| python_int_conversion_error(&port_raw))?;
    let native_catalog_path = configuration_string(
        raw.get("native_catalog_path"),
        &default_native_catalog_path(),
        "value",
    )?;
    let account_store_path =
        configuration_string(raw.get("account_store_path"), "state/accounts", "value")?;
    let account_store_path = if account_store_path.is_empty() {
        "state/accounts".to_owned()
    } else {
        account_store_path
    };
    let secret_store_path =
        configuration_string(raw.get("secret_store_path"), "state/secrets", "value")?;
    let secret_store_path = if secret_store_path.is_empty() {
        "state/secrets".to_owned()
    } else {
        secret_store_path
    };
    let codex_base_url = validate_codex_base_url(raw.get("codex_base_url"))?;
    if !(1..=65535).contains(&port) {
        return Err(ConfigError::new("port must be between 1 and 65535"));
    }

    let mut accounts = Vec::new();
    for account in python_iterable(raw.get("accounts"))? {
        accounts.push(normalize_account(&account).map_err(account_error)?);
    }
    let account_ids = accounts
        .iter()
        .map(|account| account["id"].as_str().expect("normalized account id"))
        .collect::<BTreeSet<_>>();
    let account_prefixes = accounts
        .iter()
        .map(|account| {
            account["prefix"]
                .as_str()
                .expect("normalized account prefix")
        })
        .collect::<BTreeSet<_>>();
    if account_ids.len() != accounts.len() {
        return Err(ConfigError::new("account ids must be unique"));
    }
    if account_prefixes.len() != accounts.len() {
        return Err(ConfigError::new("account prefixes must be unique"));
    }

    let mut providers = Vec::new();
    for provider in python_iterable(raw.get("providers"))? {
        providers.push(normalize_provider(&provider)?);
    }
    let mut models = Vec::new();
    for model in python_iterable(raw.get("models"))? {
        models.push(normalize_model(&model)?);
    }
    let provider_ids = providers
        .iter()
        .map(|provider| provider["id"].as_str().expect("normalized provider id"))
        .collect::<BTreeSet<_>>();
    if provider_ids.len() != providers.len() {
        return Err(ConfigError::new("provider ids must be unique"));
    }
    let conflicts = account_prefixes
        .intersection(&provider_ids)
        .copied()
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        return Err(ConfigError::new(format!(
            "account prefixes conflict with provider ids: {}",
            conflicts.join(", ")
        )));
    }
    let model_ids = models
        .iter()
        .map(|model| model["id"].as_str().expect("normalized model id"))
        .collect::<BTreeSet<_>>();
    if model_ids.len() != models.len() {
        return Err(ConfigError::new("model ids must be unique"));
    }
    let missing = models
        .iter()
        .map(|model| {
            model["provider"]
                .as_str()
                .expect("normalized model provider")
        })
        .filter(|provider| !provider_ids.contains(provider))
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        return Err(ConfigError::new(format!(
            "models reference unknown providers: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }

    let native_hidden_models =
        normalize_hidden_models(raw.get("native_hidden_models"), "native_hidden_models")
            .map_err(account_error)?;
    let native_model_context_windows =
        normalize_context_windows(raw.get("native_model_context_windows"))
            .map_err(account_error)?;
    let catalog_presentations = normalize_catalog_presentations(raw.get("catalog_presentations"))?;
    let catalog_family_presentations =
        normalize_catalog_presentations(raw.get("catalog_family_presentations"))?;
    let subscription_search = normalize_subscription_search(raw.get("subscription_search"))?;
    let codex_runtime_sources = if raw.contains_key("codex_runtime_sources") {
        normalize_codex_runtime_sources(raw.get("codex_runtime_sources"))?
    } else {
        normalize_codex_runtime_sources(Some(&serde_json::json!(["auto"])))?
    };

    Ok(serde_json::json!({
        "host": host,
        "port": port,
        "native_catalog_path": native_catalog_path,
        "account_store_path": account_store_path,
        "secret_store_path": secret_store_path,
        "codex_base_url": codex_base_url,
        "accounts": accounts,
        "providers": providers,
        "models": models,
        "native_hidden_models": native_hidden_models,
        "native_model_context_windows": native_model_context_windows,
        "catalog_presentations": catalog_presentations,
        "catalog_family_presentations": catalog_family_presentations,
        "subscription_search": subscription_search,
        "codex_runtime_sources": codex_runtime_sources,
    }))
}
