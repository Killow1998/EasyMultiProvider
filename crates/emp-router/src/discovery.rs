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
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use url::Url;

use super::{RouterError, RouterErrorKind};
use crate::official_registry::enrich_discovered_models;

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

/// Fetch the limits for a single Gemini model, matching the UI's metadata
/// operation. Other provider families do not publish this standard endpoint.
pub async fn model_metadata(
    client: &HttpClient,
    provider: &Map<String, Value>,
    model: &str,
) -> Result<Value, RouterError> {
    model_metadata_with_timeout(client, provider, model, MODEL_METADATA_TIMEOUT).await
}

async fn model_metadata_with_timeout(
    client: &HttpClient,
    provider: &Map<String, Value>,
    model: &str,
    timeout: Duration,
) -> Result<Value, RouterError> {
    let host = provider
        .get("base_url")
        .and_then(Value::as_str)
        .and_then(|base| Url::parse(base).ok())
        .and_then(|url| url.host_str().map(str::to_owned));
    if host.as_deref() != Some("generativelanguage.googleapis.com") {
        return Err(metadata_error(
            RouterErrorKind::InvalidRequest,
            400,
            FailureClass::RouterError,
            "该 Provider 没有可自动读取的模型上限，请手动填写",
        ));
    }
    let key = provider
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| {
            let id = provider
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            metadata_error(
                RouterErrorKind::MissingCredential,
                503,
                FailureClass::Upstream5xx,
                format!("API key is not configured for provider: {id}"),
            )
        })?;
    let base = provider["base_url"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('/');
    let base = base.strip_suffix("/openai").unwrap_or(base);
    let url = format!("{base}/models/{}", percent_encode_path_segment(model));
    let headers = BTreeMap::from([("x-goog-api-key".to_owned(), key.to_owned())]);
    let operation = async {
        let response = client
            .open_following_redirects(HttpMethod::Get, &url, headers, None, false)
            .await
            .map_err(|error| {
                let message = if matches!(
                    error.kind(),
                    HttpTransportErrorKind::ConnectTimeout | HttpTransportErrorKind::ReadTimeout
                ) {
                    "上游模型信息查询失败: timed out".to_owned()
                } else {
                    format!("上游模型信息查询失败: {error}")
                };
                metadata_error(
                    RouterErrorKind::Transport,
                    502,
                    FailureClass::Upstream5xx,
                    message,
                )
            })?;
        if response.status() != 200 {
            let content_type = response
                .header("content-type")
                .unwrap_or("unknown content type");
            return Err(metadata_error(
                RouterErrorKind::Upstream,
                response.status(),
                status_error_class(Some(response.status())),
                format!(
                    "上游模型信息查询失败 {} ({content_type})",
                    response.status()
                ),
            ));
        }
        let raw = response
            .read_limited(MAX_DISCOVERY_BODY_BYTES)
            .await
            .map_err(|error| {
                let message = match error.kind() {
                    HttpTransportErrorKind::ResponseTooLarge => {
                        "upstream 模型信息响应 is too large".to_owned()
                    }
                    HttpTransportErrorKind::ConnectTimeout
                    | HttpTransportErrorKind::ReadTimeout => {
                        "上游模型信息查询失败: timed out".to_owned()
                    }
                    _ => format!("上游模型信息查询失败: {error}"),
                };
                metadata_error(
                    RouterErrorKind::Protocol,
                    502,
                    FailureClass::Upstream5xx,
                    message,
                )
            })?;
        let value: Value = serde_json::from_slice(&raw).map_err(|error| {
            metadata_error(
                RouterErrorKind::Protocol,
                502,
                FailureClass::Upstream5xx,
                format!(
                    "上游模型信息查询失败: {}",
                    python_json_error_message(&raw, &error)
                ),
            )
        })?;
        if !value.is_object() {
            return Err(metadata_error(
                RouterErrorKind::Protocol,
                500,
                FailureClass::Upstream5xx,
                "internal server error",
            ));
        }
        Ok(value)
    };
    let value = tokio::time::timeout(timeout, operation)
        .await
        .map_err(|_| {
            metadata_error(
                RouterErrorKind::Transport,
                502,
                FailureClass::Upstream5xx,
                "上游模型信息查询失败: timed out",
            )
        })??;
    let metadata = value.as_object().expect("metadata object checked");
    let input = value
        .get("inputTokenLimit")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    let output = value
        .get("outputTokenLimit")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    let (Some(input), Some(output)) = (input, output) else {
        return Err(metadata_error(
            RouterErrorKind::Protocol,
            502,
            FailureClass::Upstream5xx,
            "上游未返回有效的上下文上限，请手动填写",
        ));
    };
    let (supports_reasoning, reasoning_levels) = advertised_reasoning(metadata);
    let supports_summaries = advertised_reasoning_summaries(metadata);
    Ok(json!({
        "model":model,
        "context_window":input,
        "input_token_limit":input,
        "output_token_limit":output,
        "supports_reasoning":supports_reasoning,
        "supports_reasoning_summaries":supports_summaries,
        "reasoning_levels":reasoning_levels,
    }))
}

fn metadata_error(
    kind: RouterErrorKind,
    status: u16,
    error_class: FailureClass,
    message: impl Into<String>,
) -> RouterError {
    RouterError::new(kind, status, error_class, None, None, message)
}

fn python_json_error_message(input: &[u8], error: &serde_json::Error) -> String {
    let input = String::from_utf8_lossy(input);
    let detail = error.to_string();
    let (message, position) = if detail.starts_with("expected ident") {
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

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

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
    let headers = bearer_discovery_headers(key);
    let mut budget = DiscoveryBudget::new();
    let value = get_json(client, &format!("{base}/models"), headers, &mut budget).await?;
    project_generic_models(&value).map(|models| enrich_discovered_models(provider, models))
}

pub async fn discover_gemini_models(
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
    let key = required_key(provider)?;
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
    let base = base.strip_suffix("/openai").unwrap_or(base);
    let headers = gemini_discovery_headers(key);
    let mut budget = DiscoveryBudget::new();
    let mut result = Vec::new();
    let mut page_token = None::<String>;
    for _ in 0..MAX_DISCOVERY_PAGES {
        let url = page_token.as_ref().map_or_else(
            || format!("{base}/models"),
            |token| format!("{base}/models?pageToken={}", quote_query(token)),
        );
        let value = get_json(client, &url, headers.clone(), &mut budget).await?;
        append_gemini_models(&value, &mut result)?;
        page_token = pagination_token(value.get("nextPageToken"))?;
        if page_token.is_none() {
            break;
        }
    }
    Ok(enrich_discovered_models(provider, result))
}

pub fn project_gemini_models(value: &Map<String, Value>) -> Result<Vec<Value>, RouterError> {
    let mut result = Vec::new();
    append_gemini_models(value, &mut result)?;
    Ok(result)
}

fn append_gemini_models(
    value: &Map<String, Value>,
    result: &mut Vec<Value>,
) -> Result<(), RouterError> {
    let items = value
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for item in items {
        if result.len() >= MAX_DISCOVERED_MODELS {
            return Err(model_count_error());
        }
        let Some(item) = item.as_object() else {
            continue;
        };
        if item
            .get("supportedGenerationMethods")
            .and_then(Value::as_array)
            .is_some_and(|methods| {
                !methods.is_empty()
                    && !methods
                        .iter()
                        .any(|method| method.as_str() == Some("generateContent"))
            })
        {
            continue;
        }
        let Some(model_id) = model_id(item.get("name")) else {
            continue;
        };
        let (supports_reasoning, reasoning_levels) = advertised_reasoning(item);
        let supports_summaries = advertised_reasoning_summaries(item);
        let raw_input =
            non_null(item.get("inputModalities")).or_else(|| item.get("supportedInputModalities"));
        let raw_output = non_null(item.get("outputModalities"))
            .or_else(|| item.get("supportedOutputModalities"));
        let input_limit = positive_int(item.get("inputTokenLimit"));
        let output_limit = positive_int(item.get("outputTokenLimit"));
        let display_name = model_text(item.get("displayName"), &model_id, "display name")?;
        let description = model_text(item.get("description"), "", "description")?;
        result.push(json!({
            "upstream_id": model_id,
            "display_name": display_name,
            "description": description,
            "context_window": input_limit,
            "max_input_tokens": input_limit,
            "output_limit": output_limit,
            "supports_reasoning": supports_reasoning,
            "supports_reasoning_summaries": supports_summaries,
            "reasoning_levels": reasoning_levels,
            "input_modalities": normalize_input_modalities(raw_input),
            "output_modalities": normalize_output_modalities(raw_output),
            "supports_image_detail_original": false,
            "capability_sources": {
                "supports_reasoning": source(if supports_reasoning.is_some() { "advertised" } else { "unknown" }),
                "supports_reasoning_summaries": source(if supports_summaries.is_some() { "advertised" } else { "unknown" }),
                "reasoning_levels": source(if reasoning_levels.is_empty() { "unknown" } else { "advertised" }),
                "input_modalities": source(input_modalities_metadata_source(raw_input)),
                "output_modalities": source(output_modalities_metadata_source(raw_output)),
                "supports_image_detail_original": source("unknown"),
                "context_window": source(if input_limit > 0 { "advertised" } else { "unknown" }),
                "max_input_tokens": source(if input_limit > 0 { "advertised" } else { "unknown" }),
                "output_limit": source(if output_limit > 0 { "advertised" } else { "unknown" }),
            },
            "created_at": created_timestamp(first_truthy(item, &["created", "created_at", "updated_at"])),
        }));
    }
    Ok(())
}

pub async fn discover_anthropic_models(
    client: &HttpClient,
    provider: &Map<String, Value>,
) -> Result<Vec<Value>, RouterError> {
    let key = required_key(provider)?;
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
    let base = ["/messages", "/chat/completions", "/responses"]
        .into_iter()
        .find_map(|suffix| base.strip_suffix(suffix))
        .unwrap_or(base);
    let version = provider
        .get("anthropic_version")
        .and_then(Value::as_str)
        .unwrap_or("2023-06-01");
    let headers = anthropic_discovery_headers(key, version);
    let mut budget = DiscoveryBudget::new();
    let mut result = Vec::new();
    let mut url = format!("{base}/models?limit=1000");
    for _ in 0..MAX_DISCOVERY_PAGES {
        let value = get_json(client, &url, headers.clone(), &mut budget).await?;
        append_anthropic_models(&value, &mut result)?;
        let has_more = value.get("has_more").is_some_and(python_truthy);
        let after_id = value.get("last_id").filter(|value| python_truthy(value));
        if !has_more || after_id.is_none() {
            break;
        }
        let after_id = after_id
            .and_then(python_scalar)
            .ok_or_else(pagination_error)?;
        url = format!(
            "{base}/models?limit=1000&after_id={}",
            quote_query(&after_id)
        );
    }
    Ok(enrich_discovered_models(provider, result))
}

pub fn project_anthropic_models(value: &Map<String, Value>) -> Result<Vec<Value>, RouterError> {
    let mut result = Vec::new();
    append_anthropic_models(value, &mut result)?;
    Ok(result)
}

fn append_anthropic_models(
    value: &Map<String, Value>,
    result: &mut Vec<Value>,
) -> Result<(), RouterError> {
    let items = value
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for item in items {
        if result.len() >= MAX_DISCOVERED_MODELS {
            return Err(model_count_error());
        }
        let Some(item) = item.as_object() else {
            continue;
        };
        let Some(model_id) = model_id(item.get("id")) else {
            continue;
        };
        let capabilities = item
            .get("capabilities")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let thinking = nested_supported(&capabilities, "thinking");
        let effort = nested_supported(&capabilities, "effort");
        let image = nested_supported(&capabilities, "image_input");
        let pdf = nested_supported(&capabilities, "pdf_input");
        let structured_output = nested_supported(&capabilities, "structured_outputs");
        let explicit_reasoning = [thinking, effort].into_iter().flatten().collect::<Vec<_>>();
        let supports_reasoning = (!explicit_reasoning.is_empty())
            .then(|| explicit_reasoning.into_iter().any(|value| value));
        let supports_summaries = advertised_reasoning_summaries(item);
        let mut reasoning_levels = Vec::new();
        if effort == Some(true) {
            let effort = capabilities
                .get("effort")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            for level in ["low", "medium", "high", "xhigh", "max"] {
                if nested_supported(&effort, level) == Some(true) {
                    reasoning_levels.push(level.to_owned());
                }
            }
        }
        let mut projected_capabilities = Map::new();
        let mut extra_sources = Map::new();
        if let Some(supported) = structured_output {
            projected_capabilities.insert("structured_output".to_owned(), supported.into());
            extra_sources.insert("structured_output".to_owned(), source("advertised"));
        }
        let modality_evidence = image.is_some() || pdf.is_some();
        let mut raw_input = vec![Value::String("text".to_owned())];
        if image == Some(true) {
            raw_input.push(Value::String("image".to_owned()));
        }
        if pdf == Some(true) {
            raw_input.push(Value::String("pdf".to_owned()));
        }
        let raw_input = Value::Array(raw_input);
        let max_input = positive_int(item.get("max_input_tokens"));
        let max_output = positive_int(item.get("max_tokens"));
        let display_name = model_text(item.get("display_name"), &model_id, "display name")?;
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
                source(if modality_evidence {
                    "advertised"
                } else {
                    "unknown"
                }),
            ),
            (
                "output_modalities".to_owned(),
                source(output_modalities_metadata_source(None)),
            ),
            (
                "supports_image_detail_original".to_owned(),
                source("unknown"),
            ),
            (
                "context_window".to_owned(),
                source(if max_input > 0 {
                    "advertised"
                } else {
                    "unknown"
                }),
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
                source(if max_output > 0 {
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
            "description": "",
            "context_window": max_input,
            "max_input_tokens": max_input,
            "output_limit": max_output,
            "supports_reasoning": supports_reasoning,
            "supports_reasoning_summaries": supports_summaries,
            "reasoning_levels": reasoning_levels,
            "input_modalities": normalize_input_modalities(Some(&raw_input)),
            "output_modalities": normalize_output_modalities(None),
            "supports_image_detail_original": false,
            "capability_sources": capability_sources,
            "created_at": created_timestamp(item.get("created_at")),
        });
        if !projected_capabilities.is_empty() {
            entry["capabilities"] = Value::Object(projected_capabilities);
        }
        result.push(entry);
    }
    Ok(())
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
