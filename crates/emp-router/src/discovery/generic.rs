//! Generic provider model discovery and projection.

use super::*;

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
