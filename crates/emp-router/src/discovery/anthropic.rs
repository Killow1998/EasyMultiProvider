//! Anthropic provider model discovery and projection.

use super::*;

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
