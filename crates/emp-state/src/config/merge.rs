//! Config mutation for protocol observations and browser edits.

use super::{
    ConfigError, ConfigResult, MODEL_BOOLEAN_CAPABILITIES, Map, TOP_LEVEL_PROVENANCE_FIELDS, Value,
    deployment_identity, endpoint_fingerprint, json_truthy, normalize_configuration,
    normalize_input_modalities,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn utc_date_parts(seconds: u64) -> (i64, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    let year = if month <= 2 { year + 1 } else { year };
    (year, month, day)
}

pub fn observed_at_now() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = elapsed.as_secs();
    let microseconds = elapsed.subsec_micros();
    let (year, month, day) = utc_date_parts(seconds);
    let second_of_day = seconds % 86_400;
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    if microseconds == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
    } else {
        format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{microseconds:06}+00:00"
        )
    }
}

/// Return a normalized configuration with one successful automatic protocol
/// observation applied. Explicit providers and missing identities are left
/// unchanged, matching the Python server's request-completion callback.
pub fn remember_resolved_protocol_at(
    config: &Value,
    provider_id: &str,
    requested_model: &str,
    protocol: &str,
    observed_at: &str,
) -> ConfigResult<Option<Value>> {
    if !matches!(
        protocol,
        "responses" | "chat_completions" | "anthropic_messages"
    ) {
        return Ok(None);
    }
    let mut updated = config.clone();
    let Some(root) = updated.as_object_mut() else {
        return Ok(None);
    };
    let Some(providers) = root.get("providers").and_then(Value::as_array) else {
        return Ok(None);
    };
    let Some(provider_index) = providers.iter().position(|provider| {
        provider.get("id").and_then(Value::as_str) == Some(provider_id)
            && provider.get("protocol").and_then(Value::as_str) == Some("auto")
    }) else {
        return Ok(None);
    };

    let model_index = root
        .get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models.iter().position(|model| {
                model.get("id").and_then(Value::as_str) == Some(requested_model)
                    && model.get("provider").and_then(Value::as_str) == Some(provider_id)
            })
        });
    let provider = providers[provider_index]
        .as_object()
        .expect("normalized providers are objects")
        .clone();
    let model = model_index
        .and_then(|index| root.get("models")?.as_array()?.get(index))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let upstream_model = model
        .get("upstream_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or(requested_model);
    let observation = serde_json::json!({
        "source": "observed",
        "confidence": 1.0,
        "observed_at": observed_at,
        "endpoint_fingerprint": endpoint_fingerprint(
            provider.get("base_url").and_then(Value::as_str)
        ),
        "deployment_identity": deployment_identity(&provider, &model),
        "upstream_model": upstream_model,
    });

    let provider = root
        .get_mut("providers")
        .and_then(Value::as_array_mut)
        .and_then(|providers| providers.get_mut(provider_index))
        .and_then(Value::as_object_mut)
        .expect("normalized providers are objects");
    provider.insert(
        "resolved_protocol".to_owned(),
        Value::String(protocol.to_owned()),
    );
    provider.insert("protocol_observation".to_owned(), observation.clone());
    if let Some(index) = model_index
        && let Some(model) = root
            .get_mut("models")
            .and_then(Value::as_array_mut)
            .and_then(|models| models.get_mut(index))
            .and_then(Value::as_object_mut)
    {
        model.insert(
            "resolved_protocol".to_owned(),
            Value::String(protocol.to_owned()),
        );
        model.insert("protocol_observation".to_owned(), observation);
    }
    normalize_configuration(Some(&updated)).map(Some)
}

pub fn remember_resolved_protocol(
    config: &Value,
    provider_id: &str,
    requested_model: &str,
    protocol: &str,
) -> ConfigResult<Option<Value>> {
    remember_resolved_protocol_at(
        config,
        provider_id,
        requested_model,
        protocol,
        &observed_at_now(),
    )
}

fn values_by_id(raw: Option<&Value>) -> Map<String, Value> {
    raw.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            Some((id.to_owned(), item.clone()))
        })
        .collect()
}

fn manual_provenance(observed_at: String) -> Value {
    serde_json::json!({
        "source": "manual",
        "confidence": 1.0,
        "observed_at": observed_at,
    })
}

fn unknown_provenance() -> Value {
    serde_json::json!({
        "source": "unknown",
        "confidence": 0.0,
        "observed_at": Value::Null,
    })
}

fn merge_web_update_at_time(
    current: &Value,
    incoming: &Value,
    mut observed_at: impl FnMut() -> String,
) -> ConfigResult<Value> {
    let Value::Object(_) = incoming else {
        return Err(ConfigError::new("request body must be an object"));
    };
    let mut merged = incoming.clone();
    let merged_object = merged.as_object_mut().expect("checked object");

    merged_object.insert(
        "codex_runtime_sources".to_owned(),
        current
            .get("codex_runtime_sources")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![Value::String("auto".to_owned())])),
    );
    for field in [
        "account_store_path",
        "secret_store_path",
        "native_catalog_path",
    ] {
        if let Some(value) = current.get(field) {
            merged_object.insert(field.to_owned(), value.clone());
        }
    }

    let old_providers = values_by_id(current.get("providers"));
    if let Some(Value::Array(providers)) = merged_object.get_mut("providers") {
        for provider_value in providers {
            let Some(provider) = provider_value.as_object_mut() else {
                continue;
            };
            let old = provider
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| old_providers.get(id));
            let masked = provider
                .get("api_key")
                .is_some_and(|value| value == "••••••••");
            if !provider.contains_key("api_key") || masked {
                provider.insert(
                    "api_key".to_owned(),
                    old.and_then(|value| value.get("api_key"))
                        .cloned()
                        .unwrap_or_else(|| Value::String(String::new())),
                );
            }
            let incoming_file = provider.get("api_key_file");
            let old_file = old.and_then(|value| value.get("api_key_file"));
            if incoming_file.is_some_and(json_truthy) && incoming_file != old_file {
                return Err(ConfigError::new("provider.api_key_file is managed by EMP"));
            }
            if old_file.is_some_and(json_truthy) {
                provider.insert(
                    "api_key_file".to_owned(),
                    old_file.expect("truthy old file").clone(),
                );
            }
            if old.is_some_and(|old| {
                ["base_url", "protocol", "deployment_identity"]
                    .iter()
                    .any(|field| provider.get(*field) != old.get(*field))
            }) {
                provider.insert("resolved_protocol".to_owned(), Value::String(String::new()));
                provider.insert("protocol_observation".to_owned(), Value::Object(Map::new()));
            }
        }
    }

    let old_accounts = values_by_id(current.get("accounts"));
    if let Some(Value::Array(accounts)) = merged_object.get_mut("accounts") {
        for account_value in accounts {
            let Some(account) = account_value.as_object_mut() else {
                continue;
            };
            let old = account
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| old_accounts.get(id));
            let incoming_file = account.get("auth_file");
            let old_file = old.and_then(|value| value.get("auth_file"));
            if incoming_file.is_some_and(json_truthy) && incoming_file != old_file {
                return Err(ConfigError::new("account.auth_file is managed by EMP"));
            }
            if old_file.is_some_and(json_truthy) {
                account.insert(
                    "auth_file".to_owned(),
                    old_file.expect("truthy old file").clone(),
                );
            }
        }
    }

    let old_models = values_by_id(current.get("models"));
    if let Some(Value::Array(models)) = merged_object.get_mut("models") {
        for model_value in models {
            let Some(model) = model_value.as_object_mut() else {
                continue;
            };
            let old = model
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| old_models.get(id));
            model.insert(
                "context_calibrations".to_owned(),
                old.and_then(|value| value.get("context_calibrations"))
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new())),
            );
            if let Some(old) = old {
                for field in [
                    "visibility",
                    "supports_reasoning",
                    "supports_reasoning_summaries",
                    "input_modalities",
                    "output_modalities",
                    "supported_protocols",
                    "reasoning_control",
                    "max_input_tokens",
                    "output_limit",
                    "supports_image_detail_original",
                    "deployment_identity",
                    "resolved_protocol",
                    "protocol_observation",
                ] {
                    if !model.contains_key(field) {
                        model.insert(
                            field.to_owned(),
                            old.get(field).cloned().unwrap_or(Value::Null),
                        );
                    }
                }
            }

            let old_capabilities = old
                .and_then(|value| value.get("capabilities"))
                .filter(|value| json_truthy(value));
            match (old_capabilities, model.get_mut("capabilities")) {
                (Some(old_capabilities), Some(Value::Object(incoming_capabilities))) => {
                    for field in MODEL_BOOLEAN_CAPABILITIES {
                        if !incoming_capabilities.contains_key(field)
                            && let Some(value) = old_capabilities.get(field)
                        {
                            incoming_capabilities.insert(field.to_owned(), value.clone());
                        }
                    }
                }
                (Some(old_capabilities), _) => {
                    model.insert("capabilities".to_owned(), old_capabilities.clone());
                }
                (None, _) => {}
            }

            let mut sources = old
                .and_then(|value| value.get("capability_sources"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let incoming_sources = model
                .get("capability_sources")
                .and_then(Value::as_object)
                .cloned();
            if let Some(incoming_sources) = &incoming_sources {
                sources.extend(incoming_sources.clone());
            }
            for field in TOP_LEVEL_PROVENANCE_FIELDS {
                if !model.contains_key(field) {
                    continue;
                }
                let changed = old.is_none_or(|old| model.get(field) != old.get(field));
                if !changed {
                    continue;
                }
                let requested_source = incoming_sources
                    .as_ref()
                    .and_then(|values| values.get(field))
                    .and_then(Value::as_object)
                    .and_then(|value| value.get("source"))
                    .and_then(Value::as_str);
                let provenance = if field == "input_modalities"
                    && requested_source == Some("unknown")
                    && !normalize_input_modalities(model.get(field))
                        .iter()
                        .any(|modality| modality == "image")
                {
                    unknown_provenance()
                } else {
                    manual_provenance(observed_at())
                };
                sources.insert(field.to_owned(), provenance);
            }

            let old_capabilities = old
                .and_then(|value| value.get("capabilities"))
                .and_then(Value::as_object);
            if let Some(capabilities) = model.get("capabilities").and_then(Value::as_object) {
                for field in MODEL_BOOLEAN_CAPABILITIES {
                    let Some(value) = capabilities.get(field) else {
                        continue;
                    };
                    if old.is_none()
                        || old_capabilities.and_then(|values| values.get(field)) != Some(value)
                    {
                        sources.insert(field.to_owned(), manual_provenance(observed_at()));
                    }
                }
            }
            if !sources.is_empty() {
                model.insert("capability_sources".to_owned(), Value::Object(sources));
            }
            if old.is_some_and(|old| {
                ["provider", "upstream_id", "deployment_identity"]
                    .iter()
                    .any(|field| model.get(*field) != old.get(*field))
            }) {
                model.insert("resolved_protocol".to_owned(), Value::String(String::new()));
                model.insert("protocol_observation".to_owned(), Value::Object(Map::new()));
            }
        }
    }
    normalize_configuration(Some(&merged))
}

/// Apply a Web update while preserving secrets and managed discovery metadata.
///
/// This is Python `merge_web_update` with its optional filesystem path omitted;
/// canonical private-path validation remains a separate state transition.
pub fn merge_web_update(current: &Value, incoming: &Value) -> ConfigResult<Value> {
    merge_web_update_at_time(current, incoming, observed_at_now)
}

/// Apply the same merge with a fixed timestamp for deterministic compatibility tests.
pub fn merge_web_update_with_time(
    current: &Value,
    incoming: &Value,
    observed_at: &str,
) -> ConfigResult<Value> {
    merge_web_update_at_time(current, incoming, || observed_at.to_owned())
}
