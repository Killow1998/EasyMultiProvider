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
    let mut headers = common_discovery_headers();
    headers.insert("x-goog-api-key".to_owned(), key.to_owned());
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
        let (input_modalities, input_source) = discovered_modalities(raw_input);
        let (output_modalities, output_source) = discovered_modalities(raw_output);
        let input_limit = positive_int(item.get("inputTokenLimit"));
        let output_limit = positive_int(item.get("outputTokenLimit"));
        let display_name = model_text(item.get("displayName"), &model_id)?;
        let description = model_text(item.get("description"), "")?;
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
            "input_modalities": input_modalities,
            "output_modalities": output_modalities,
            "supports_image_detail_original": false,
            "capability_sources": {
                "supports_reasoning": declared_source(supports_reasoning.is_some()),
                "supports_reasoning_summaries": declared_source(supports_summaries.is_some()),
                "reasoning_levels": declared_source(!reasoning_levels.is_empty()),
                "input_modalities": input_source,
                "output_modalities": output_source,
                "supports_image_detail_original": source("unknown"),
                "context_window": if input_limit > 0 { json!({"source":"advertised","input_only":true}) } else { source("unknown") },
                "max_input_tokens": declared_source(input_limit > 0),
                "output_limit": declared_source(output_limit > 0),
            },
            "created_at": created_timestamp(first_truthy(item, &["created", "created_at", "updated_at"])),
        }));
    }
    Ok(())
}
