use emp_domain::model_metadata::ModelMetadata;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

pub(super) type Catalog = BTreeMap<String, BTreeMap<String, ModelMetadata>>;

pub(super) fn from_litellm(entries: &Map<String, Value>) -> Catalog {
    let mut models = Catalog::new();
    let provider_of = |entry: &Value| -> Option<String> {
        if !matches!(entry["mode"].as_str(), Some("chat" | "responses")) {
            return None;
        }
        Some(
            match entry["litellm_provider"].as_str()? {
                "vertex_ai-language-models" | "vertex_ai-anthropic_models" => "vertex_ai",
                other => other,
            }
            .to_owned(),
        )
    };
    for (key, entry) in entries {
        let Some(provider) = provider_of(entry) else {
            continue;
        };
        let id = match key.split_once('/') {
            Some((prefix, id)) if prefix == provider => id,
            _ if entries
                .get(&format!("{provider}/{key}"))
                .and_then(provider_of)
                .as_deref()
                == Some(provider.as_str()) =>
            {
                continue;
            }
            _ => key,
        };
        let tokens = |field: &str| {
            entry[field]
                .as_u64()
                .filter(|value| (1..=100_000_000).contains(value))
        };
        let modalities = entry.get("supported_modalities");
        let output = entry.get("supported_output_modalities");
        let model = ModelMetadata {
            upstream_id: id.to_owned(),
            max_input_tokens: tokens("max_input_tokens"),
            max_output_tokens: tokens("max_output_tokens"),
            supports_reasoning: entry["supports_reasoning"].as_bool(),
            supports_vision: entry["supports_vision"].as_bool(),
            supports_audio_input: entry["supports_audio_input"].as_bool(),
            input_modalities: emp_domain::model_values::parse_modalities(modalities)
                .unwrap_or_default(),
            output_modalities: emp_domain::model_values::parse_modalities(output)
                .unwrap_or_default(),
            reasoning_levels: emp_domain::normalize_reasoning_levels(
                entry.get("reasoning_effort_levels"),
            ),
            reported_efforts: ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
                .into_iter()
                .filter(|level| entry[format!("supports_{level}_reasoning_effort")] == true)
                .map(str::to_owned)
                .collect(),
        };
        models
            .entry(provider)
            .or_default()
            .insert(id.to_owned(), model);
    }
    models
}
