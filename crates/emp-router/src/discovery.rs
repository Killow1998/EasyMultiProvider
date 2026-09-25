//! Bounded model discovery for OpenAI-compatible provider catalogs.

use std::collections::{BTreeMap, BTreeSet};

use emp_state::{
    input_modalities_metadata_source, normalize_input_modalities, normalize_output_modalities,
    normalize_reasoning_levels, output_modalities_metadata_source,
};
use emp_transport::{
    FailureClass, HttpClient, HttpMethod, HttpTransportErrorKind, status_error_class,
};
use serde_json::{Map, Value, json};
use std::time::{Duration, Instant};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};
use url::Url;

use super::{RouterError, RouterErrorKind};
use crate::official_registry::enrich_discovered_models;

mod anthropic;
mod gemini;
mod generic;
mod metadata;

pub use anthropic::{discover_anthropic_models, project_anthropic_models};
pub use gemini::{discover_gemini_models, project_gemini_models};
pub use generic::{discover_generic_models, project_generic_models};
pub use metadata::model_metadata;

pub const MAX_DISCOVERY_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DISCOVERED_MODELS: usize = 1000;
const MAX_DISCOVERY_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_DISCOVERY_FIELD_BYTES: usize = 4096;
const MAX_DISCOVERY_TOKEN_BYTES: usize = 4096;
const MAX_CONTEXT_WINDOW: u64 = 100_000_000;
const MAX_MODEL_TIMESTAMP: i64 = 4_102_444_800;
const DISCOVERY_WALL_CLOCK: Duration = Duration::from_secs(60);
const MODEL_METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_DISCOVERY_PAGES: usize = 20;

struct DiscoveryBudget {
    deadline: Instant,
    bytes: usize,
}

impl DiscoveryBudget {
    fn new() -> Self {
        Self {
            deadline: Instant::now() + DISCOVERY_WALL_CLOCK,
            bytes: 0,
        }
    }

    fn remaining(&self) -> Result<Duration, RouterError> {
        self.deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(discovery_timeout)
    }

    fn record(&mut self, count: usize) -> Result<(), RouterError> {
        self.bytes = self.bytes.checked_add(count).ok_or_else(|| {
            discovery_error(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                "provider discovery response exceeded its total limit",
            )
        })?;
        if self.bytes > MAX_DISCOVERY_TOTAL_BYTES {
            return Err(discovery_error(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                "provider discovery response exceeded its total limit",
            ));
        }
        Ok(())
    }
}

pub async fn discover_models(
    client: &HttpClient,
    provider: &Map<String, Value>,
) -> Result<Vec<Value>, RouterError> {
    let protocol = provider
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auth_mode = provider
        .get("auth_mode")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if protocol == "anthropic_messages" || (protocol == "auto" && auth_mode == "anthropic_api_key")
    {
        return discover_anthropic_models(client, provider).await;
    }
    let is_native_gemini = provider
        .get("base_url")
        .and_then(Value::as_str)
        .and_then(|value| Url::parse(value).ok())
        .and_then(|value| value.host_str().map(str::to_owned))
        .as_deref()
        == Some("generativelanguage.googleapis.com");
    if is_native_gemini {
        return discover_gemini_models(client, provider).await;
    }
    discover_generic_models(client, provider).await
}

pub(crate) fn python_json_error_message(input: &[u8], error: &serde_json::Error) -> String {
    let input = String::from_utf8_lossy(input);
    let detail = error.to_string();
    let (message, position) =
        if detail.starts_with("EOF while parsing an object") && input.trim_end().ends_with('{') {
            (
                "Expecting property name enclosed in double quotes",
                input.chars().count(),
            )
        } else if detail.starts_with("expected ident") {
            let position = input
                .chars()
                .position(|character| !character.is_whitespace())
                .unwrap_or(0);
            ("Expecting value", position)
        } else {
            let message = if detail.starts_with("expected value") {
                "Expecting value"
            } else if detail.starts_with("key must be a string") {
                "Expecting property name enclosed in double quotes"
            } else if detail.starts_with("expected `:`") {
                "Expecting ':' delimiter"
            } else if detail.starts_with("expected `,`") {
                "Expecting ',' delimiter"
            } else if detail.starts_with("trailing characters") {
                "Extra data"
            } else {
                return detail;
            };
            let line_start = input
                .split_inclusive('\n')
                .take(error.line().saturating_sub(1))
                .map(|line| line.chars().count())
                .sum::<usize>();
            let line_length = input
                .split('\n')
                .nth(error.line().saturating_sub(1))
                .map_or(0, |line| line.chars().count());
            (
                message,
                line_start + error.column().saturating_sub(1).min(line_length),
            )
        };
    let characters = input.chars().collect::<Vec<_>>();
    let position = position.min(characters.len());
    let line = characters[..position]
        .iter()
        .filter(|character| **character == '\n')
        .count()
        + 1;
    let column = characters[..position]
        .iter()
        .rev()
        .take_while(|character| **character != '\n')
        .count()
        + 1;
    format!("{message}: line {line} column {column} (char {position})")
}

fn advertised_reasoning(metadata: &Map<String, Value>) -> (Option<bool>, Vec<String>) {
    let parameter_support = metadata
        .get("supported_parameters")
        .and_then(Value::as_array)
        .is_some_and(|parameters| {
            parameters.iter().filter_map(Value::as_str).any(|value| {
                matches!(
                    value.trim().to_lowercase().as_str(),
                    "reasoning" | "reasoning_effort" | "thinking"
                )
            })
        });
    let nested = metadata
        .get("reasoning")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let raw_support = metadata
        .get("supports_reasoning")
        .or_else(|| metadata.get("reasoning_supported"))
        .or_else(|| nested.get("supported"));
    let mut support = raw_support.and_then(Value::as_bool).or_else(|| {
        (metadata.get("thinking").and_then(Value::as_bool) == Some(true) || parameter_support)
            .then_some(true)
    });
    let raw_levels = metadata
        .get("reasoning_levels")
        .or_else(|| metadata.get("supported_reasoning_levels"))
        .or_else(|| nested.get("effort_levels"))
        .or_else(|| nested.get("supported_efforts"));
    let mut levels = Vec::new();
    if let Some(raw_levels) = raw_levels.and_then(Value::as_array)
        && raw_levels.len() <= 16
    {
        let mut valid = true;
        for raw in raw_levels {
            let value = if python_truthy(raw) {
                python_scalar(raw).unwrap_or_default()
            } else {
                String::new()
            };
            let value = python_trim(&value).to_owned();
            if value.is_empty()
                || value.len() > 64
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
            {
                valid = false;
                break;
            }
            if !levels.contains(&value) {
                levels.push(value);
            }
        }
        if !valid {
            levels.clear();
        }
    }
    let levels = normalize_reasoning_levels(Some(&Value::Array(
        levels.into_iter().map(Value::String).collect(),
    )));
    if !levels.is_empty() {
        support = Some(true);
    }
    (support, levels)
}

fn advertised_reasoning_summaries(metadata: &Map<String, Value>) -> Option<bool> {
    let parameter_support = metadata
        .get("supported_parameters")
        .and_then(Value::as_array)
        .is_some_and(|parameters| {
            parameters.iter().filter_map(Value::as_str).any(|value| {
                matches!(
                    value.trim().to_lowercase().as_str(),
                    "reasoning_summary" | "reasoning.summary" | "reasoning_summary_text"
                )
            })
        });
    let nested = metadata
        .get("reasoning")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata
        .get("supports_reasoning_summaries")
        .or_else(|| metadata.get("supports_reasoning_summary_parameter"))
        .or_else(|| nested.get("supports_summary"))
        .or_else(|| nested.get("summary_supported"))
        .and_then(Value::as_bool)
        .or_else(|| parameter_support.then_some(true))
}

fn model_id(value: Option<&Value>) -> Option<String> {
    let value = value?;
    if !python_truthy(value) {
        return None;
    }
    let mut value = python_scalar(value)?;
    value = python_trim(&value).to_owned();
    if value.len() > MAX_DISCOVERY_FIELD_BYTES {
        return None;
    }
    if let Some(stripped) = value.strip_prefix("models/") {
        value = stripped.to_owned();
    }
    (!value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b':' | b'-')
        }))
    .then_some(value)
}

fn model_text(
    value: Option<&Value>,
    fallback: &str,
    _field: &'static str,
) -> Result<String, RouterError> {
    let text = value
        .filter(|value| python_truthy(value))
        .and_then(python_scalar)
        .unwrap_or_else(|| fallback.to_owned());
    if text.len() > MAX_DISCOVERY_FIELD_BYTES {
        return Err(discovery_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::ProtocolError,
            "provider model metadata exceeded its limit",
        ));
    }
    Ok(text)
}

fn positive_int(value: Option<&Value>) -> u64 {
    let Some(value) = value else {
        return 0;
    };
    if value.is_boolean() {
        return 0;
    }
    value
        .as_u64()
        .filter(|value| *value > 0 && *value <= MAX_CONTEXT_WINDOW)
        .unwrap_or(0)
}

fn created_timestamp(value: Option<&Value>) -> i64 {
    let Some(value) = value else {
        return 0;
    };
    if value.is_boolean() {
        return 0;
    }
    let number = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => python_trim(text).parse::<f64>().ok().or_else(|| {
            OffsetDateTime::parse(python_trim(text), &Rfc3339)
                .ok()
                .map(|value| value.unix_timestamp() as f64)
                .or_else(|| local_date_timestamp(python_trim(text)).map(|value| value as f64))
        }),
        _ => None,
    };
    let Some(mut number) = number.filter(|number| number.is_finite()) else {
        return 0;
    };
    if number > 100_000_000_000.0 {
        number /= 1000.0;
    }
    let timestamp = number.trunc() as i64;
    if (1..=MAX_MODEL_TIMESTAMP).contains(&timestamp) {
        timestamp
    } else {
        0
    }
}

/// Python's `datetime.fromisoformat(date).timestamp()` interprets a date-only
/// value at local midnight, including the offset in force on that date.
fn local_date_timestamp(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return None;
    }
    let year = value[..4].parse::<i32>().ok()?;
    let month = value[5..7].parse::<u8>().ok()?;
    let day = value[8..].parse::<u8>().ok()?;
    Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
    // SAFETY: `tm` is a C plain-data struct. `mktime` initializes its derived
    // fields from the supplied date and `tm_isdst = -1` asks it to infer DST.
    let mut local: libc::tm = unsafe { std::mem::zeroed() };
    local.tm_year = year - 1900;
    local.tm_mon = i32::from(month) - 1;
    local.tm_mday = i32::from(day);
    local.tm_isdst = -1;
    #[cfg(unix)]
    let seconds = unsafe { libc::mktime(&mut local) as i64 };
    #[cfg(windows)]
    let seconds = unsafe { _mktime64(&mut local) };
    #[cfg(not(any(unix, windows)))]
    let seconds = 0;
    Some(seconds)
}

#[cfg(windows)]
unsafe extern "C" {
    fn _mktime64(local: *mut libc::tm) -> i64;
}

fn first_truthy<'a>(item: &'a Map<String, Value>, fields: &[&str]) -> Option<&'a Value> {
    fields
        .iter()
        .filter_map(|field| item.get(*field))
        .find(|value| python_truthy(value))
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(
            character,
            '\t' | '\n'
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

fn python_scalar(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("None".to_owned()),
        Value::Bool(true) => Some("True".to_owned()),
        Value::Bool(false) => Some("False".to_owned()),
        Value::Number(value) => Some(value.to_string()),
        Value::String(value) => Some(value.clone()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn non_null(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

fn nested_supported(container: &Map<String, Value>, field: &str) -> Option<bool> {
    container
        .get(field)
        .and_then(Value::as_object)
        .and_then(|value| value.get("supported"))
        .and_then(Value::as_bool)
}

fn required_key(provider: &Map<String, Value>) -> Result<&str, RouterError> {
    provider
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            discovery_error(
                RouterErrorKind::MissingCredential,
                503,
                FailureClass::Auth,
                "provider API key is not configured",
            )
        })
}

fn common_discovery_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Accept".to_owned(), "application/json".to_owned()),
        (
            "User-Agent".to_owned(),
            format!("EMP/{}", env!("CARGO_PKG_VERSION")),
        ),
    ])
}

fn bearer_discovery_headers(key: &str) -> BTreeMap<String, String> {
    let mut headers = common_discovery_headers();
    headers.insert("Authorization".to_owned(), format!("Bearer {key}"));
    headers
}

fn gemini_discovery_headers(key: &str) -> BTreeMap<String, String> {
    let mut headers = common_discovery_headers();
    headers.insert("x-goog-api-key".to_owned(), key.to_owned());
    headers
}

fn anthropic_discovery_headers(key: &str, version: &str) -> BTreeMap<String, String> {
    let mut headers = common_discovery_headers();
    headers.insert("x-api-key".to_owned(), key.to_owned());
    headers.insert("anthropic-version".to_owned(), version.to_owned());
    headers
}

async fn get_json(
    client: &HttpClient,
    url: &str,
    headers: BTreeMap<String, String>,
    budget: &mut DiscoveryBudget,
) -> Result<Map<String, Value>, RouterError> {
    let response = tokio::time::timeout(
        budget.remaining()?,
        client.open(HttpMethod::Get, url, headers, None, false),
    )
    .await
    .map_err(|_| discovery_timeout())?
    .map_err(transport_error)?;
    let status = response.status();
    if !(200..300).contains(&status) {
        return Err(discovery_error(
            RouterErrorKind::Upstream,
            status,
            status_error_class(Some(status)),
            "provider discovery request failed",
        ));
    }
    let raw = tokio::time::timeout(
        budget.remaining()?,
        response.read_limited(MAX_DISCOVERY_BODY_BYTES),
    )
    .await
    .map_err(|_| discovery_timeout())?
    .map_err(read_error)?;
    budget.record(raw.len())?;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| {
        discovery_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::ProtocolError,
            "provider discovery returned invalid JSON",
        )
    })?;
    value.as_object().cloned().ok_or_else(|| {
        discovery_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::ProtocolError,
            "provider discovery returned an invalid shape",
        )
    })
}

fn transport_error(error: emp_transport::HttpTransportError) -> RouterError {
    discovery_error(
        RouterErrorKind::Transport,
        502,
        match error.kind() {
            HttpTransportErrorKind::ConnectTimeout => FailureClass::ConnectTimeout,
            HttpTransportErrorKind::ReadTimeout => FailureClass::Timeout,
            _ => FailureClass::Network,
        },
        match error.kind() {
            HttpTransportErrorKind::ConnectTimeout | HttpTransportErrorKind::ReadTimeout => {
                "provider discovery timed out"
            }
            _ => "provider discovery transport failed",
        },
    )
}

fn read_error(error: emp_transport::HttpTransportError) -> RouterError {
    match error.kind() {
        HttpTransportErrorKind::ReadTimeout => discovery_timeout(),
        HttpTransportErrorKind::ResponseTooLarge => discovery_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::ProtocolError,
            "provider discovery response exceeded its limit",
        ),
        _ => transport_error(error),
    }
}

fn discovery_timeout() -> RouterError {
    discovery_error(
        RouterErrorKind::Transport,
        504,
        FailureClass::Timeout,
        "provider discovery timed out",
    )
}

fn model_count_error() -> RouterError {
    discovery_error(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        "provider model list exceeded its limit",
    )
}

fn pagination_error() -> RouterError {
    discovery_error(
        RouterErrorKind::Protocol,
        502,
        FailureClass::ProtocolError,
        "provider pagination token is invalid",
    )
}

fn pagination_token(value: Option<&Value>) -> Result<Option<String>, RouterError> {
    let Some(value) = value.filter(|value| python_truthy(value)) else {
        return Ok(None);
    };
    let token = value.as_str().ok_or_else(pagination_error)?;
    if token.len() > MAX_DISCOVERY_TOKEN_BYTES {
        return Err(pagination_error());
    }
    Ok(Some(token.to_owned()))
}

fn quote_query(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            result.push(char::from(byte));
        } else {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            result.push('%');
            result.push(char::from(HEX[usize::from(byte >> 4)]));
            result.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    result
}

fn source(source: &str) -> Value {
    json!({"source": source})
}

fn discovery_error(
    kind: RouterErrorKind,
    status: u16,
    error_class: FailureClass,
    message: &'static str,
) -> RouterError {
    RouterError::new(
        kind,
        status,
        error_class,
        Some("model_discovery_failed".to_owned()),
        None,
        message,
    )
}
