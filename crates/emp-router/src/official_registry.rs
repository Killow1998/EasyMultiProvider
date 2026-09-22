//! Release-bundled official capability enrichment, equivalent to Python EMP.

use std::sync::OnceLock;

use serde_json::{Map, Value, json};
use url::Url;

const TOP_LEVEL_FIELDS: [&str; 9] = [
    "context_window",
    "max_input_tokens",
    "input_modalities",
    "output_modalities",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "web_search",
];
const PROJECTED_FIELDS: [(&str, &str, bool); 6] = [
    ("max_output_tokens", "output_limit", false),
    ("protocols", "supported_protocols", false),
    ("tool_calling", "structured_tools", true),
    ("parallel_tool_calling", "parallel_tools", true),
    ("streaming", "streaming", true),
    ("structured_output", "structured_output", true),
];

fn registry() -> &'static Map<String, Value> {
    static REGISTRY: OnceLock<Map<String, Value>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str::<Value>(include_str!("../data/official_models.json"))
            .expect("bundled official model registry must be valid JSON")
            .as_object()
            .expect("bundled official model registry must be an object")
            .clone()
    })
}

pub fn identify_provider(provider: &Map<String, Value>) -> Option<String> {
    let registry = registry();
    let providers = registry.get("providers")?.as_array()?;
    let normalized = normalize_url(provider.get("base_url")?.as_str()?);
    if normalized.is_empty() {
        return None;
    }
    if let Some(explicit) = provider
        .get("official_provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return providers
            .iter()
            .filter_map(Value::as_object)
            .find(|record| record.get("key").and_then(Value::as_str) == Some(explicit))
            .filter(|record| provider_urls(record).iter().any(|url| url == &normalized))
            .map(|_| explicit.to_owned());
    }
    providers
        .iter()
        .filter_map(Value::as_object)
        .find(|record| provider_urls(record).iter().any(|url| url == &normalized))
        .and_then(|record| record.get("key"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

pub fn enrich_discovered_models(provider: &Map<String, Value>, models: Vec<Value>) -> Vec<Value> {
    let Some(provider_key) = identify_provider(provider) else {
        return models;
    };
    let registry = registry();
    let observed_at = registry
        .get("reviewed_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let provenance = json!({
        "source": "official",
        "confidence": 0.95,
        "observed_at": observed_at,
    });
    let registry_models = registry
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    models
        .into_iter()
        .map(|model| {
            let Some(mut model) = model.as_object().cloned() else {
                return model;
            };
            let Some(model_id) = ["upstream_id", "model_id", "id"]
                .into_iter()
                .filter_map(|field| model.get(field))
                .find(|value| python_truthy(value))
                .and_then(Value::as_str)
            else {
                return Value::Object(model);
            };
            let Some(official) = registry_models
                .iter()
                .filter_map(Value::as_object)
                .filter(|entry| {
                    entry.get("provider_key").and_then(Value::as_str) == Some(provider_key.as_str())
                })
                .find(|entry| model_matches(entry, model_id))
            else {
                return Value::Object(model);
            };
            let mut sources = model
                .get("capability_sources")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            for field in TOP_LEVEL_FIELDS {
                let official_value = official.get(field).unwrap_or(&Value::Null);
                if is_missing(official_value) {
                    continue;
                }
                let current = model.get(field).unwrap_or(&Value::Null);
                if can_override(current, &sources, field) {
                    model.insert(field.to_owned(), official_value.clone());
                    sources.insert(field.to_owned(), provenance.clone());
                }
            }

            for (registry_field, target, nested) in PROJECTED_FIELDS {
                let official_value = official.get(registry_field).unwrap_or(&Value::Null);
                if is_missing(official_value) {
                    continue;
                }
                if nested {
                    let current_capabilities = model
                        .get("capabilities")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let current = current_capabilities.get(target).unwrap_or(&Value::Null);
                    if can_override(current, &sources, target) {
                        let mut capabilities = current_capabilities;
                        capabilities.insert(target.to_owned(), official_value.clone());
                        model.insert("capabilities".to_owned(), Value::Object(capabilities));
                        sources.insert(target.to_owned(), provenance.clone());
                    }
                } else {
                    let current = model.get(target).unwrap_or(&Value::Null);
                    if can_override(current, &sources, target) {
                        model.insert(target.to_owned(), official_value.clone());
                        sources.insert(target.to_owned(), provenance.clone());
                    }
                }
            }

            if !sources.is_empty() {
                model.insert("capability_sources".to_owned(), Value::Object(sources));
            }
            Value::Object(model)
        })
        .collect()
}

fn model_matches(model: &Map<String, Value>, candidate: &str) -> bool {
    model.get("model_id").and_then(Value::as_str) == Some(candidate)
        || model
            .get("aliases")
            .and_then(Value::as_array)
            .is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| alias.as_str() == Some(candidate))
            })
}

fn can_override(current: &Value, sources: &Map<String, Value>, field: &str) -> bool {
    if is_missing(current) {
        return true;
    }
    let source = sources.get(field).and_then(|value| {
        value.as_str().or_else(|| {
            value
                .as_object()
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str)
        })
    });
    matches!(source, Some("unknown" | "inferred" | "official"))
}

fn is_missing(value: &Value) -> bool {
    value.is_null()
        || value.as_str().is_some_and(str::is_empty)
        || value.as_array().is_some_and(Vec::is_empty)
}

fn provider_urls(provider: &Map<String, Value>) -> Vec<String> {
    provider
        .get("api_base_urls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(normalize_url)
        .filter(|value| !value.is_empty())
        .collect()
}

fn normalize_url(value: &str) -> String {
    let Ok(value) = Url::parse(value.trim()) else {
        return String::new();
    };
    if !matches!(value.scheme(), "http" | "https")
        || !value.username().is_empty()
        || value.password().is_some()
        || value.query().is_some()
        || value.fragment().is_some()
    {
        return String::new();
    }
    let Some(host) = value.host_str() else {
        return String::new();
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    let port = value
        .port()
        .map_or_else(String::new, |port| format!(":{port}"));
    let path = if value.path().len() > 1 {
        value.path().trim_end_matches('/').to_owned()
    } else {
        "/".to_owned()
    };
    format!("{}://{host}{port}{path}", value.scheme())
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
