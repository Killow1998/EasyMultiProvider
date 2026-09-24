//! One model record and its effective capability values.

use super::*;

/// Normalize one complete model record exactly like Python `_normalize_model`.
///
/// The result always has the 26 configured output fields; unknown model fields
/// are intentionally discarded. Capability-source normalization sees the
/// union of raw top-level and nested capability keys, but uses the normalized
/// record for effective capability values.
pub fn normalize_model(raw: &Value) -> ConfigResult<Value> {
    let Value::Object(raw) = raw else {
        return Err(ConfigError::new("each model must be an object"));
    };
    let levels = strict_reasoning_levels(raw.get("reasoning_levels"))?;
    let raw_reasoning_support = raw.get("supports_reasoning");
    let supports_reasoning = match raw_reasoning_support {
        None | Some(Value::Null) => {
            if levels.is_empty() {
                Value::Null
            } else {
                Value::Bool(true)
            }
        }
        Some(Value::Bool(value)) => Value::Bool(*value),
        Some(_) => {
            return Err(ConfigError::new(
                "model.supports_reasoning must be boolean or null",
            ));
        }
    };
    let supports_reasoning_summaries = match raw.get("supports_reasoning_summaries") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::Bool(value)) => Value::Bool(*value),
        Some(_) => {
            return Err(ConfigError::new(
                "model.supports_reasoning_summaries must be boolean or null",
            ));
        }
    };
    let context_window = model_python_int_or_zero(raw.get("context_window"))?;
    if context_window < 0 {
        return Err(ConfigError::new("model.context_window cannot be negative"));
    }
    if context_window > MAX_CONTEXT_WINDOW {
        return Err(ConfigError::new("model.context_window is too large"));
    }
    let output_raw = if raw.contains_key("output_limit") {
        raw.get("output_limit")
    } else {
        raw.get("output_token_limit")
    };
    let output_limit = model_python_int_or_zero(output_raw)?;
    if output_limit < 0 {
        return Err(ConfigError::new("model.output_limit cannot be negative"));
    }
    if output_limit > MAX_CONTEXT_WINDOW {
        return Err(ConfigError::new("model.output_limit is too large"));
    }
    let created_at = normalize_created_at(raw.get("created_at"))?;
    let visibility = string_value(raw.get("visibility"), "model.visibility", false)?;
    let visibility = if visibility.is_empty() {
        "list".to_owned()
    } else if visibility == "list" || visibility == "hide" {
        visibility
    } else {
        return Err(ConfigError::new("model.visibility must be list or hide"));
    };
    let supports_image_detail_original = match raw.get("supports_image_detail_original") {
        Some(Value::Bool(value)) => *value,
        _ => false,
    };
    let max_input_tokens = model_python_int_or_zero(raw.get("max_input_tokens"))?;
    if max_input_tokens < 0 {
        return Err(ConfigError::new(
            "model.max_input_tokens cannot be negative",
        ));
    }
    if max_input_tokens > MAX_CONTEXT_WINDOW {
        return Err(ConfigError::new("model.max_input_tokens is too large"));
    }
    let reasoning_control = string_value(raw.get("reasoning_control"), "value", false)?;
    let output_modalities = normalize_output_modalities(raw.get("output_modalities"));
    let supported_protocols = normalize_supported_protocols(raw.get("supported_protocols"));
    let mut model = serde_json::json!({
        "id": broad_model_id(raw.get("id"), "model.id")?,
        "provider": broad_model_id(raw.get("provider"), "model.provider")?,
        "upstream_id": string_value(raw.get("upstream_id"), "value", false)?,
        "family_id": safe_capability_identity(
            raw.get("family_id"),
            "model.family_id",
        )?,
        "display_name": string_value(raw.get("display_name"), "value", false)?,
        "description": string_value(raw.get("description"), "value", false)?,
        "supports_reasoning": supports_reasoning,
        "supports_reasoning_summaries": supports_reasoning_summaries,
        "reasoning_levels": levels,
        "reasoning_control": reasoning_control,
        "context_window": context_window,
        "max_input_tokens": max_input_tokens,
        "output_limit": output_limit,
        "created_at": created_at,
        "enabled": raw.get("enabled").is_none_or(json_truthy),
        "visibility": visibility,
        "input_modalities": normalize_input_modalities(raw.get("input_modalities")),
        "output_modalities": output_modalities,
        "supported_protocols": supported_protocols,
        "supports_image_detail_original": supports_image_detail_original,
        "deployment_identity": safe_capability_identity(
            raw.get("deployment_identity"),
            "model.deployment_identity",
        )?,
        "resolved_protocol": normalize_resolved_protocol(
            raw.get("resolved_protocol"),
        )?,
        "protocol_observation": normalize_protocol_observation(
            raw.get("protocol_observation"),
        )?,
        "context_calibrations": normalize_context_calibrations(
            raw.get("context_calibrations"),
        )?,
        "capabilities": normalized_capability_object(
            raw.get("capabilities"),
            "model.capabilities",
        )?,
    });
    let mut explicit_fields = raw.keys().map(String::as_str).collect::<Vec<_>>();
    if let Some(Value::Object(capabilities)) = raw.get("capabilities") {
        explicit_fields.extend(capabilities.keys().map(String::as_str));
    }
    model["capability_sources"] = normalize_model_capability_sources(
        raw.get("capability_sources"),
        &model,
        Some(&explicit_fields),
    )?;
    Ok(model)
}
