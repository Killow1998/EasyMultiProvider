use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Deserialize, Serialize)]
pub struct VertexModelMetadata {
    pub upstream_id: String,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_reasoning: Option<bool>,
    pub supports_vision: Option<bool>,
    pub supports_audio_input: Option<bool>,
}

pub fn is_vertex_text_model(id: &str) -> bool {
    id.starts_with("gemini-")
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        && ![
            "-embedding",
            "-image",
            "-tts",
            "-audio",
            "-live",
            "-transcribe",
        ]
        .iter()
        .any(|kind| id.contains(kind))
}

pub(super) fn from_litellm(payload: &Value) -> Vec<VertexModelMetadata> {
    let mut models = BTreeMap::new();
    for (key, entry) in payload.as_object().expect("validated LiteLLM catalog") {
        if entry["mode"] != "chat"
            || !matches!(
                entry["litellm_provider"].as_str(),
                Some("vertex_ai" | "vertex_ai-language-models")
            )
        {
            continue;
        }
        let id = key.strip_prefix("vertex_ai/").unwrap_or(key);
        if !is_vertex_text_model(id) {
            continue;
        }
        let tokens = |field: &str| {
            entry[field]
                .as_u64()
                .filter(|value| (1..=100_000_000).contains(value))
        };
        models.insert(
            id,
            VertexModelMetadata {
                upstream_id: id.into(),
                max_input_tokens: tokens("max_input_tokens"),
                max_output_tokens: tokens("max_output_tokens"),
                supports_reasoning: entry["supports_reasoning"].as_bool(),
                supports_vision: entry["supports_vision"].as_bool(),
                supports_audio_input: entry["supports_audio_input"].as_bool(),
            },
        );
    }
    models.into_values().collect()
}
