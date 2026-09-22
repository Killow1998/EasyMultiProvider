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
use std::time::Duration;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{RouterError, RouterErrorKind};

pub const MAX_DISCOVERY_BODY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_DISCOVERED_MODELS: usize = 1000;
const MAX_DISCOVERY_FIELD_BYTES: usize = 4096;
const MAX_CONTEXT_WINDOW: u64 = 100_000_000;
const MAX_MODEL_TIMESTAMP: i64 = 4_102_444_800;
const DISCOVERY_WALL_CLOCK: Duration = Duration::from_secs(60);

pub async fn discover_generic_models(
    client: &HttpClient,
    provider: &Map<String, Value>,
) -> Result<Vec<Value>, RouterError> {
    if provider.get("auth_mode").and_then(Value::as_str) != Some("api_key") {
        return Err(discovery_error(
            RouterErrorKind::InvalidRequest,
            400,
            FailureClass::RouterError,
            "provider discovery requires an API key",
        ));
    }
    let key = provider
        .get("api_key")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if key.is_empty() {
        return Err(discovery_error(
            RouterErrorKind::MissingCredential,
            503,
            FailureClass::Auth,
            "provider API key is not configured",
        ));
    }
    let base = provider
        .get("base_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('/');
    if base.is_empty() {
        return Err(discovery_error(
            RouterErrorKind::InvalidRequest,
            400,
            FailureClass::RouterError,
            "provider base URL is missing",
        ));
    }
    let base = ["/chat/completions", "/responses"]
        .into_iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);
    let mut headers = BTreeMap::new();
    headers.insert("Accept".to_owned(), "application/json".to_owned());
    headers.insert(
        "User-Agent".to_owned(),
        format!("EMP/{}", env!("CARGO_PKG_VERSION")),
    );
    headers.insert("Authorization".to_owned(), format!("Bearer {key}"));
    let raw = tokio::time::timeout(DISCOVERY_WALL_CLOCK, async {
        let response = client
            .open(
                HttpMethod::Get,
                &format!("{base}/models"),
                headers,
                None,
                false,
            )
            .await
            .map_err(|error| {
                discovery_error(
                    RouterErrorKind::Transport,
                    502,
                    match error.kind() {
                        HttpTransportErrorKind::ConnectTimeout => FailureClass::ConnectTimeout,
                        HttpTransportErrorKind::ReadTimeout => FailureClass::Timeout,
                        _ => FailureClass::Network,
                    },
                    match error.kind() {
                        HttpTransportErrorKind::ConnectTimeout
                        | HttpTransportErrorKind::ReadTimeout => "provider discovery timed out",
                        _ => "provider discovery transport failed",
                    },
                )
            })?;
        let status = response.status();
        if !(200..300).contains(&status) {
            return Err(discovery_error(
                RouterErrorKind::Upstream,
                status,
                status_error_class(Some(status)),
                "provider discovery request failed",
            ));
        }
        response
            .read_limited(MAX_DISCOVERY_BODY_BYTES)
            .await
            .map_err(|error| {
                let timed_out = error.kind() == HttpTransportErrorKind::ReadTimeout;
                discovery_error(
                    if timed_out {
                        RouterErrorKind::Transport
                    } else {
                        RouterErrorKind::Protocol
                    },
                    if timed_out { 504 } else { 502 },
                    if timed_out {
                        FailureClass::Timeout
                    } else {
                        FailureClass::ProtocolError
                    },
                    if timed_out {
                        "provider discovery timed out"
                    } else {
                        "provider discovery response exceeded its limit"
                    },
                )
            })
    })
    .await
    .map_err(|_| {
        discovery_error(
            RouterErrorKind::Transport,
            504,
            FailureClass::Timeout,
            "provider discovery timed out",
        )
    })??;
    let value: Value = serde_json::from_slice(&raw).map_err(|_| {
        discovery_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::ProtocolError,
            "provider discovery returned invalid JSON",
        )
    })?;
    let value = value.as_object().ok_or_else(|| {
        discovery_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::ProtocolError,
            "provider discovery returned an invalid shape",
        )
    })?;
    project_generic_models(value)
}

pub fn project_generic_models(value: &Map<String, Value>) -> Result<Vec<Value>, RouterError> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut result = Vec::new();
    for item in items {
        if result.len() >= MAX_DISCOVERED_MODELS {
            return Err(discovery_error(
                RouterErrorKind::Protocol,
                502,
                FailureClass::ProtocolError,
                "provider model list exceeded its limit",
            ));
        }
        let Some(item) = item.as_object() else {
            continue;
        };
        let Some(model_id) = model_id(item.get("id")) else {
            continue;
        };
        let context = positive_int(first_truthy(
            item,
            &["context_window", "context_length", "inputTokenLimit"],
        ));
        let architecture = item
            .get("architecture")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let raw_input = architecture.get("input_modalities");
        let raw_output = architecture.get("output_modalities");
        let raw_image_detail = item
            .get("supports_image_detail_original")
            .or_else(|| architecture.get("supports_image_detail_original"));
        let supports_image_detail = raw_image_detail.and_then(Value::as_bool).unwrap_or(false);
        let (supports_reasoning, reasoning_levels) = advertised_reasoning(item);
        let supports_summaries = advertised_reasoning_summaries(item);
        let parameters = item
            .get("supported_parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|value| python_trim(value).to_lowercase())
            .collect::<BTreeSet<_>>();
        let mut capabilities = Map::new();
        let mut extra_sources = Map::new();
        for (parameter, field) in [
            ("tools", "structured_tools"),
            ("parallel_tool_calls", "parallel_tools"),
        ] {
            if parameters.contains(parameter) {
                capabilities.insert(field.to_owned(), Value::Bool(true));
                extra_sources.insert(field.to_owned(), source("advertised"));
            }
        }
        if parameters.contains("structured_outputs") || parameters.contains("response_format") {
            capabilities.insert("structured_output".to_owned(), Value::Bool(true));
            extra_sources.insert("structured_output".to_owned(), source("advertised"));
        }
        if let Some(streaming) = item.get("streaming").and_then(Value::as_bool) {
            capabilities.insert("streaming".to_owned(), Value::Bool(streaming));
            extra_sources.insert("streaming".to_owned(), source("advertised"));
        }
        let output_limit = ["output_limit", "max_tokens", "max_output_tokens"]
            .into_iter()
            .find_map(|field| {
                let value = positive_int(item.get(field));
                (value > 0).then_some(value)
            })
            .unwrap_or_else(|| {
                item.get("top_provider")
                    .and_then(Value::as_object)
                    .map(|provider| positive_int(provider.get("max_completion_tokens")))
                    .unwrap_or(0)
            });
        let max_input = positive_int(item.get("max_input_tokens"));
        let display_value = first_truthy(item, &["display_name", "name"]);
        let display_name = model_text(display_value, &model_id, "display name")?;
        let description = model_text(item.get("description"), "", "description")?;
        let input_modalities = normalize_input_modalities(raw_input);
        let output_modalities = normalize_output_modalities(raw_output);
        let mut capability_sources = Map::from_iter([
            (
                "supports_reasoning".to_owned(),
                source(if supports_reasoning.is_some() {
                    "advertised"
                } else {
                    "unknown"
                }),
            ),
            (
                "supports_reasoning_summaries".to_owned(),
                source(if supports_summaries.is_some() {
                    "advertised"
                } else {
                    "unknown"
                }),
            ),
            (
                "reasoning_levels".to_owned(),
                source(if reasoning_levels.is_empty() {
                    "unknown"
                } else {
                    "advertised"
                }),
            ),
            (
                "input_modalities".to_owned(),
                source(input_modalities_metadata_source(raw_input)),
            ),
            (
                "output_modalities".to_owned(),
                source(output_modalities_metadata_source(raw_output)),
            ),
            (
                "supports_image_detail_original".to_owned(),
                source(if raw_image_detail.is_some_and(Value::is_boolean) {
                    "advertised"
                } else {
                    "unknown"
                }),
            ),
            (
                "context_window".to_owned(),
                source(if context > 0 { "advertised" } else { "unknown" }),
            ),
            (
                "max_input_tokens".to_owned(),
                source(if max_input > 0 {
                    "advertised"
                } else {
                    "unknown"
                }),
            ),
            (
                "output_limit".to_owned(),
                source(if output_limit > 0 {
                    "advertised"
                } else {
                    "unknown"
                }),
            ),
        ]);
        capability_sources.extend(extra_sources);
        let mut entry = json!({
            "upstream_id": model_id,
            "display_name": display_name,
            "description": description,
            "context_window": context,
            "max_input_tokens": max_input,
            "output_limit": output_limit,
            "supports_reasoning": supports_reasoning,
            "supports_reasoning_summaries": supports_summaries,
            "reasoning_levels": reasoning_levels,
            "input_modalities": input_modalities,
            "output_modalities": output_modalities,
            "supports_image_detail_original": supports_image_detail,
            "capability_sources": capability_sources,
            "created_at": created_timestamp(first_truthy(item, &["created", "created_at", "updated_at"])),
        });
        if !capabilities.is_empty() {
            entry["capabilities"] = Value::Object(capabilities);
        }
        result.push(entry);
    }
    Ok(result)
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
