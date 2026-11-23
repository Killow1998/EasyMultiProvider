use super::*;

pub async fn discover_anthropic_models(
    client: &HttpClient,
    provider: &Map<String, Value>,
    official_models: &OfficialModels,
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
    let mut headers = common_discovery_headers();
    headers.insert("x-api-key".to_owned(), key.to_owned());
    headers.insert("anthropic-version".to_owned(), version.to_owned());
    let mut budget = DiscoveryBudget::new();
    let mut result = Vec::new();
    let mut url = format!("{base}/models?limit=1000");
    for _ in 0..MAX_DISCOVERY_PAGES {
        let value = get_json(client, &url, headers.clone(), &mut budget).await?;
        append_anthropic_models(&value, &mut result)?;
        let has_more = value.get("has_more").is_some_and(json_truthy);
        let after_id = value.get("last_id").filter(|value| json_truthy(value));
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
    Ok(official_models.enrich_discovered_models(provider, result))
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
        let capabilities = item.get("capabilities").unwrap_or(&Value::Null);
        let thinking = nested_supported(capabilities, "thinking");
        let effort = nested_supported(capabilities, "effort");
        let image = nested_supported(capabilities, "image_input");
        let pdf = nested_supported(capabilities, "pdf_input");
        let structured_output = nested_supported(capabilities, "structured_outputs");
        let supports_reasoning = match (thinking, effort) {
            (None, None) => None,
            (thinking, effort) => Some(thinking == Some(true) || effort == Some(true)),
        };
        let supports_summaries = advertised_reasoning_summaries(item);
        let mut reasoning_levels = Vec::new();
        if effort == Some(true) {
            let effort = &capabilities["effort"];
            for level in ["low", "medium", "high", "xhigh", "max"] {
                if nested_supported(effort, level) == Some(true) {
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
        let mut raw_input = vec!["text"];
        if image == Some(true) {
            raw_input.push("image");
        }
        if pdf == Some(true) {
            raw_input.push("pdf");
        }
        let max_input = positive_int(item.get("max_input_tokens"));
        let max_output = positive_int(item.get("max_tokens"));
        let display_name = model_text(item.get("display_name"), &model_id)?;
        let mut capability_sources = Map::from_iter([
            (
                "supports_reasoning".to_owned(),
                declared_source(supports_reasoning.is_some()),
            ),
            (
                "supports_reasoning_summaries".to_owned(),
                declared_source(supports_summaries.is_some()),
            ),
            (
                "reasoning_levels".to_owned(),
                declared_source(!reasoning_levels.is_empty() || effort == Some(false)),
            ),
            (
                "input_modalities".to_owned(),
                declared_source(modality_evidence),
            ),
            ("output_modalities".to_owned(), source("unknown")),
            (
                "supports_image_detail_original".to_owned(),
                source("unknown"),
            ),
            (
                "context_window".to_owned(),
                if max_input > 0 {
                    json!({"source":"advertised","input_only":true})
                } else {
                    source("unknown")
                },
            ),
            (
                "max_input_tokens".to_owned(),
                declared_source(max_input > 0),
            ),
            ("output_limit".to_owned(), declared_source(max_output > 0)),
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
            "input_modalities": raw_input,
            "output_modalities": ["text"],
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
