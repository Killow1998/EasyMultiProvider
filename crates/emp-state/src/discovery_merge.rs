//! Selected discovery results merged by the Python capability provenance policy.
use crate::{
    ConfigError, input_modalities_metadata_source, normalize_configuration,
    normalize_input_modalities, normalize_output_modalities, normalize_supported_protocols,
    output_modalities_metadata_source,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;

const SCALAR_FIELDS: [&str; 7] = [
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "supports_image_detail_original",
];
const LIST_FIELDS: [&str; 4] = [
    "reasoning_levels",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
];
const NESTED_FIELDS: [&str; 6] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "websocket",
];
const SOURCES: [(&str, f64); 7] = [
    ("unknown", 0.0),
    ("inherited", 0.6),
    ("inferred", 0.35),
    ("official", 0.95),
    ("advertised", 0.75),
    ("observed", 1.0),
    ("manual", 1.0),
];

#[derive(Debug)]
pub struct DiscoveryMerge {
    pub config: Value,
    pub available: usize,
    pub added: usize,
    pub hidden: usize,
}

pub fn merge_selected_models(
    config: &Value,
    provider_id: &str,
    discovered: &[Value],
    selected: &Value,
    observed_at: &str,
) -> Result<DiscoveryMerge, ConfigError> {
    let selected = selected
        .as_array()
        .filter(|items| items.iter().all(Value::is_string))
        .ok_or_else(|| ConfigError::new("selected models must be a list of model IDs"))?;
    let selected = selected
        .iter()
        .filter_map(Value::as_str)
        .collect::<BTreeSet<_>>();
    let available = discovered
        .iter()
        .filter_map(|item| item.get("upstream_id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if !selected.is_subset(&available) {
        return Err(ConfigError::new(
            "selected model is not in the discovered list",
        ));
    }
    let mut models = config
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut hidden = 0;
    for model in &mut models {
        let upstream = model
            .get("upstream_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if model.get("provider").and_then(Value::as_str) == Some(provider_id)
            && available.contains(upstream)
            && !selected.contains(upstream)
            && model.get("enabled").is_none_or(truthy)
        {
            model["enabled"] = false.into();
            hidden += 1;
        }
    }
    let mut added = 0;
    for item in discovered {
        let upstream = item
            .get("upstream_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !selected.contains(upstream) {
            continue;
        }
        let model_id = format!("{provider_id}/{upstream}");
        if let Some(existing) = models
            .iter_mut()
            .rev()
            .find(|model| model.get("id").and_then(Value::as_str) == Some(&model_id))
        {
            merge_existing(existing, item, observed_at);
        } else if !upstream.is_empty() {
            models.push(build_new_model(item, provider_id, observed_at));
            added += 1;
        }
    }
    let mut updated = config.clone();
    updated["models"] = Value::Array(models);
    Ok(DiscoveryMerge {
        config: normalize_configuration(Some(&updated))?,
        available: discovered.len(),
        added,
        hidden,
    })
}

fn merge_existing(existing: &mut Value, item: &Value, observed_at: &str) {
    for field in SCALAR_FIELDS.into_iter().chain(LIST_FIELDS) {
        let Some(value) = meaningful_field(item, field) else {
            continue;
        };
        let incoming = incoming_source(item, field);
        if source_rank(incoming) < source_rank(field_source(existing, field)) {
            continue;
        }
        existing[field] = match field {
            "input_modalities" => json!(normalize_input_modalities(Some(value))),
            "output_modalities" => json!(normalize_output_modalities(Some(value))),
            "supported_protocols" => json!(normalize_supported_protocols(Some(value))),
            _ => value.clone(),
        };
        set_source(existing, field, incoming, observed_at);
    }
    for field in NESTED_FIELDS {
        let Some(value) = item
            .get("capabilities")
            .and_then(|caps| caps.get(field))
            .filter(|value| value.is_boolean())
        else {
            continue;
        };
        let incoming = if has_source(item, field) {
            field_source(item, field)
        } else {
            "advertised"
        };
        if source_rank(incoming) < source_rank(field_source(existing, field)) {
            continue;
        }
        if !existing.get("capabilities").is_some_and(Value::is_object) {
            existing["capabilities"] = json!({});
        }
        existing["capabilities"][field] = value.clone();
        set_source(existing, field, incoming, observed_at);
    }
    for field in ["created_at", "family_id"] {
        if let Some(value) = item.get(field).filter(|value| truthy(value))
            && !existing.get(field).is_some_and(truthy)
        {
            existing[field] = value.clone();
        }
    }
}

pub fn build_new_model(item: &Value, provider: &str, observed_at: &str) -> Value {
    let upstream = item
        .get("upstream_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut capabilities = Map::new();
    let mut sources = Map::new();
    for field in NESTED_FIELDS {
        if let Some(value) = item
            .get("capabilities")
            .and_then(|caps| caps.get(field))
            .filter(|value| value.is_boolean())
        {
            capabilities.insert(field.to_owned(), value.clone());
            let source = if has_source(item, field) {
                field_source(item, field)
            } else {
                "advertised"
            };
            sources.insert(field.to_owned(), provenance(source, observed_at));
        }
    }
    for field in SCALAR_FIELDS.into_iter().chain(LIST_FIELDS) {
        if meaningful_field(item, field).is_some() {
            sources.insert(
                field.to_owned(),
                provenance(incoming_source(item, field), observed_at),
            );
        }
    }
    let mut model = json!({
        "id": format!("{provider}/{upstream}"), "provider": provider, "upstream_id": upstream,
        "family_id": item.get("family_id").cloned().unwrap_or(json!("")),
        "display_name": item.get("display_name").filter(|value| truthy(value)).cloned().unwrap_or(json!(upstream)),
        "description": item.get("description").cloned().unwrap_or(json!("")),
        "supports_reasoning": item.get("supports_reasoning").and_then(Value::as_bool),
        "supports_reasoning_summaries": item.get("supports_reasoning_summaries").and_then(Value::as_bool),
        "reasoning_levels": item.get("reasoning_levels").filter(|value| truthy(value)).cloned().unwrap_or(json!([])),
        "reasoning_control": item.get("reasoning_control").cloned().unwrap_or(json!("")),
        "context_window": integer(item.get("context_window")),
        "max_input_tokens": integer(item.get("max_input_tokens")),
        "output_limit": integer(item.get("output_limit").or_else(|| item.get("output_token_limit"))),
        "created_at": integer(item.get("created_at")), "enabled": true, "visibility": "list",
        "input_modalities": normalize_input_modalities(item.get("input_modalities")),
        "output_modalities": normalize_output_modalities(item.get("output_modalities")),
        "supported_protocols": normalize_supported_protocols(item.get("supported_protocols")),
        "supports_image_detail_original": item.get("supports_image_detail_original").and_then(Value::as_bool).unwrap_or(false),
        "capability_sources": sources,
    });
    if !capabilities.is_empty() {
        model["capabilities"] = Value::Object(capabilities);
    }
    model
}

fn integer(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Number(value)) => value
            .as_i64()
            .unwrap_or_else(|| value.as_f64().unwrap_or(0.0) as i64),
        Some(Value::String(value)) => value.trim().parse().unwrap_or(0),
        Some(Value::Bool(value)) => i64::from(*value),
        _ => 0,
    }
}

fn meaningful_field<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    let value = item.get(field).filter(|value| !value.is_null())?;
    if value.as_str().is_some_and(str::is_empty)
        || (value.as_array().is_some_and(Vec::is_empty) && !has_source(item, field))
    {
        return None;
    }
    Some(value)
}

fn incoming_source<'a>(item: &'a Value, field: &str) -> &'a str {
    if has_source(item, field) {
        return field_source(item, field);
    }
    match field {
        "context_window"
        | "max_input_tokens"
        | "output_limit"
        | "reasoning_levels"
        | "supported_protocols" => {
            if item.get(field).is_some_and(truthy) {
                "advertised"
            } else {
                "unknown"
            }
        }
        "input_modalities" => input_modalities_metadata_source(item.get(field)),
        "output_modalities" => output_modalities_metadata_source(item.get(field)),
        _ => "unknown",
    }
}
fn has_source(item: &Value, field: &str) -> bool {
    item.get("capability_sources")
        .and_then(Value::as_object)
        .is_some_and(|sources| sources.contains_key(field))
}
fn field_source<'a>(item: &'a Value, field: &str) -> &'a str {
    let source = item
        .get("capability_sources")
        .and_then(|sources| sources.get(field))
        .and_then(|entry| {
            entry
                .as_str()
                .or_else(|| entry.get("source").and_then(Value::as_str))
        })
        .unwrap_or("unknown");
    if SOURCES.iter().any(|(known, _)| *known == source) {
        source
    } else {
        "unknown"
    }
}
fn source_rank(source: &str) -> usize {
    SOURCES
        .iter()
        .position(|(known, _)| *known == source)
        .unwrap_or(0)
}
fn provenance(source: &str, observed_at: &str) -> Value {
    json!({"source": source, "confidence": SOURCES[source_rank(source)].1, "observed_at": observed_at})
}
fn set_source(model: &mut Value, field: &str, source: &str, observed_at: &str) {
    if !model
        .get("capability_sources")
        .is_some_and(Value::is_object)
    {
        model["capability_sources"] = json!({});
    }
    model["capability_sources"][field] = provenance(source, observed_at);
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
