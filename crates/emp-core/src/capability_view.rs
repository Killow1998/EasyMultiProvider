//! Credential-free capability records from normalized EMP configuration.
use crate::{deployment_identity, endpoint_fingerprint};
use serde_json::{Map, Value, json};

pub fn safe_id(value: Option<&Value>, fallback: &str) -> String {
    let Some(text) = value.and_then(Value::as_str).map(str::trim).filter(|s| {
        !s.is_empty()
            && s.len() <= 256
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b':' | b'-'))
    }) else {
        return fallback.to_owned();
    };
    text.to_owned()
}
pub fn confidence(source: &str) -> f64 {
    match source {
        "official" => 0.95,
        "advertised" => 0.75,
        "observed" | "manual" => 1.0,
        "inherited" => 0.6,
        "inferred" => 0.35,
        _ => 0.0,
    }
}
fn provenance(raw: Option<&Value>, default: &str) -> Value {
    let source = raw
        .and_then(|v| v.get("source"))
        .and_then(Value::as_str)
        .unwrap_or(default);
    let known = matches!(
        source,
        "official" | "advertised" | "observed" | "manual" | "inherited" | "inferred" | "unknown"
    );
    let weight = raw
        .and_then(|v| v.get("confidence"))
        .filter(|v| !v.is_null())
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .unwrap_or(confidence(source));
    if !known || !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
        return json!({"source":"unknown","confidence":0.0,"observed_at":null});
    }
    json!({"source":source,"confidence":weight,"observed_at":raw.and_then(|v|v.get("observed_at"))})
}
fn capability(value: Value, source: &Map<String, Value>, field: &str, known: bool) -> Value {
    let mut result = provenance(
        source.get("capability_sources").and_then(|v| v.get(field)),
        if known { "inferred" } else { "unknown" },
    );
    if !known || result["source"] == "unknown" {
        result["source"] = json!("unknown");
        result["confidence"] = json!(0.0);
        result["value"] = json!("unknown");
    } else {
        result["value"] = value;
    }
    result
}
fn lookup(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
    names: &[&str],
) -> (Value, Map<String, Value>) {
    for source in [model, provider] {
        for name in names {
            if let Some(value) = source.get(*name) {
                return (value.clone(), source.clone());
            }
            if let Some(value) = source
                .get("capabilities")
                .and_then(|nested| nested.get(*name))
            {
                if let Some(content) = value.get("value") {
                    return (
                        content.clone(),
                        Map::from_iter([("capability_sources".into(), json!({*name:value}))]),
                    );
                }
                return (value.clone(), source.clone());
            }
        }
    }
    (Value::Null, model.clone())
}
fn concrete(value: &str) -> bool {
    matches!(
        value,
        "responses" | "chat_completions" | "anthropic_messages"
    )
}
fn encoded(text: &str) -> String {
    let mut result = String::new();
    use std::fmt::Write;
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            result.push(byte as char);
        } else {
            write!(result, "%{byte:02X}").expect("string write");
        }
    }
    result
}

pub fn capability_record(provider: &Map<String, Value>, model: &Map<String, Value>) -> Value {
    let fingerprint = endpoint_fingerprint(provider.get("base_url").and_then(Value::as_str));
    let upstream = safe_id(
        model
            .get("upstream_id")
            .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
            .or_else(|| model.get("id")),
        "unknown",
    );
    let deployment = deployment_identity(provider, model);
    let configured = provider
        .get("protocol")
        .and_then(Value::as_str)
        .filter(|v| concrete(v) || *v == "auto")
        .unwrap_or("unknown");
    let mut resolved = "unknown";
    let mut observed = provenance(None, "unknown");
    for source in [model, provider] {
        let Some(protocol) = source
            .get("resolved_protocol")
            .and_then(Value::as_str)
            .filter(|v| concrete(v))
        else {
            continue;
        };
        let Some(observation) = source.get("protocol_observation").filter(|v| v.is_object()) else {
            continue;
        };
        if [
            ("endpoint_fingerprint", fingerprint.as_str()),
            ("upstream_model", upstream.as_str()),
            ("deployment_identity", deployment.as_str()),
        ]
        .iter()
        .any(|(field, expected)| {
            observation
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s != *expected)
        }) {
            continue;
        }
        observed = provenance(Some(observation), "observed");
        resolved = if observed["source"] == "unknown" {
            "unknown"
        } else {
            protocol
        };
        break;
    }
    let effective = if configured == "auto" {
        resolved
    } else {
        configured
    };
    let configured_source = if configured == "unknown" {
        "unknown"
    } else {
        "manual"
    };
    let mut capabilities = Map::new();
    capabilities.insert("configured_protocol".into(),json!({"value":configured,"source":configured_source,"confidence":confidence(configured_source),"observed_at":null}));
    let mut effective_value = if effective == resolved {
        observed
    } else {
        json!({"source":configured_source,"confidence":1.0,"observed_at":null})
    };
    effective_value["value"] = json!(effective);
    capabilities.insert("effective_protocol".into(), effective_value);
    for (field, names) in [
        (
            "streaming",
            &["streaming", "supports_streaming", "supports_stream"][..],
        ),
        (
            "structured_tools",
            &[
                "structured_tools",
                "supports_structured_tools",
                "supports_tools",
            ][..],
        ),
        (
            "parallel_tools",
            &["parallel_tools", "supports_parallel_tools"][..],
        ),
        ("websocket", &["websocket", "supports_websocket"][..]),
        (
            "structured_output",
            &["structured_output", "supports_structured_output"][..],
        ),
        ("web_search", &["web_search", "supports_web_search"][..]),
    ] {
        let (value, source) = lookup(provider, model, names);
        let known = value.is_boolean();
        capabilities.insert(field.into(), capability(value, &source, field, known));
    }
    for field in [
        "supports_reasoning",
        "supports_reasoning_summaries",
        "supports_image_detail_original",
        "reasoning_levels",
        "reasoning_control",
        "context_window",
        "max_input_tokens",
        "output_limit",
        "input_modalities",
        "output_modalities",
        "supported_protocols",
    ] {
        let mut value = model.get(field).cloned().unwrap_or(Value::Null);
        let known = match field {
            "context_window" | "max_input_tokens" | "output_limit" => {
                value.as_u64().is_some_and(|v| v > 0)
            }
            "reasoning_levels"
            | "input_modalities"
            | "output_modalities"
            | "supported_protocols" => {
                if field == "supported_protocols"
                    && let Some(items) = value.as_array_mut()
                {
                    items.retain(|v| v.as_str().is_some_and(concrete));
                }
                value.as_array().is_some_and(|v| !v.is_empty())
            }
            "reasoning_control" => value.as_str().is_some_and(|v| !v.trim().is_empty()),
            _ => value.is_boolean(),
        };
        capabilities.insert(field.into(), capability(value, model, field, known));
    }
    let protocol = if effective == "unknown" {
        configured
    } else {
        effective
    };
    json!({"key":{"endpoint_fingerprint":fingerprint,"upstream_model":upstream,"protocol_identity":protocol,"deployment_identity":deployment},
        "key_id":format!("cap:v1:{fingerprint}:{}:{}:{}",encoded(&upstream),encoded(protocol),encoded(&deployment)),
        "provider_id":safe_id(provider.get("id"),"unknown"),"model_id":safe_id(model.get("id"),"unknown"),"capabilities":capabilities})
}
