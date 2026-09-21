//! Exact, bounded configuration helpers shared with the Python implementation.
//!
//! Full account, provider, model, path and persistence normalization remains a
//! later migration slice. Exposing only completed helpers prevents callers from
//! mistaking a partial configuration rebuild for the production contract.

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
