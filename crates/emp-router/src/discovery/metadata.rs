//! Gemini model-specific limit lookup used by the management UI.
use super::*;

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
