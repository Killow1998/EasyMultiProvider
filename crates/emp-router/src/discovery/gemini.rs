//! Gemini provider model discovery and projection.

use super::*;

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
