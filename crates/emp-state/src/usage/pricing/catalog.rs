use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct ModelMetadata {
    pub upstream_id: String,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_reasoning: Option<bool>,
    pub supports_vision: Option<bool>,
    pub supports_audio_input: Option<bool>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<String>,
    #[serde(default)]
    pub reported_efforts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_levels: Vec<String>,
}

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
            input_modalities: crate::model_values::parse_modalities(modalities).unwrap_or_default(),
            output_modalities: crate::model_values::parse_modalities(output).unwrap_or_default(),
            reasoning_levels: crate::normalize_reasoning_levels(
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

impl ModelMetadata {
    pub fn apply(&self, model: &mut Value) {
        let modalities = if self.input_modalities.is_empty() {
            self.supports_vision.map(|vision| {
                let mut inputs = vec!["text"];
                if vision {
                    inputs.push("image");
                }
                if self.supports_audio_input == Some(true) {
                    inputs.push("audio");
                }
                json!(inputs)
            })
        } else {
            Some(json!(self.input_modalities))
        };
        if self.reported_efforts.is_empty() {
            model
                .as_object_mut()
                .expect("discovered model")
                .remove("catalog_efforts");
        } else {
            model["catalog_efforts"] = json!(self.reported_efforts);
        }
        for (field, value) in [
            (
                "reasoning_levels",
                (!self.reasoning_levels.is_empty()).then(|| json!(self.reasoning_levels)),
            ),
            ("context_window", self.max_input_tokens.map(Value::from)),
            ("max_input_tokens", self.max_input_tokens.map(Value::from)),
            ("output_limit", self.max_output_tokens.map(Value::from)),
            ("input_modalities", modalities),
            (
                "output_modalities",
                (!self.output_modalities.is_empty()).then(|| json!(self.output_modalities)),
            ),
            (
                "supports_reasoning",
                self.supports_reasoning
                    .or_else(|| {
                        (!self.reasoning_levels.is_empty()
                            || self.reported_efforts.iter().any(|level| level != "none"))
                        .then_some(true)
                    })
                    .map(Value::from),
            ),
        ] {
            if let Some(value) = value
                && (model[field].is_null()
                    || matches!(
                        model["capability_sources"][field]["source"].as_str(),
                        Some("inferred" | "unknown" | "catalog")
                    ))
            {
                model[field] = value;
                model["capability_sources"][field] = if field == "context_window" {
                    json!({"source":"catalog","input_only":true})
                } else {
                    json!({"source":"catalog"})
                };
            }
        }
    }
}

pub fn apply_catalog_metadata(models: &mut [Value], catalog: &[ModelMetadata]) {
    let by_id: BTreeMap<_, _> = catalog
        .iter()
        .map(|entry| (entry.upstream_id.as_str(), entry))
        .collect();
    for model in models {
        if let Some(entry) = model["upstream_id"].as_str().and_then(|id| by_id.get(id)) {
            entry.apply(model);
        }
    }
}
