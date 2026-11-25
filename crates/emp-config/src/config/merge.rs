use super::provider_url::{hostname, parse_provider_url};
use super::{
    ConfigError, ConfigResult, MASKED_API_KEY, MODEL_BOOLEAN_CAPABILITIES, Map,
    TOP_LEVEL_PROVENANCE_FIELDS, Value, json_truthy, normalize_configuration_with_registry,
    normalize_input_modalities, parse_observed_at,
};
use emp_domain::{Protocol, ProtocolObservationKey, ResolvedRoute, RouteSource};
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

pub struct LearnedProtocol {
    pub provider_id: String,
    pub requested_model: String,
    model_id: String,
    source: RouteSource,
    key: ProtocolObservationKey,
    protocol: Protocol,
    observed_at: String,
}

impl LearnedProtocol {
    pub fn from_route(
        route: &ResolvedRoute,
        protocol: Protocol,
        observed_at: String,
    ) -> Option<Self> {
        if route.protocol != Protocol::Auto || protocol == Protocol::Auto {
            return None;
        }
        let model = route.model.value();
        Some(Self {
            provider_id: route.provider_id.clone(),
            requested_model: route.requested_model.clone(),
            model_id: model.get("id")?.as_str()?.to_owned(),
            source: route.source,
            key: ProtocolObservationKey::from_route(route),
            protocol,
            observed_at,
        })
    }
}

pub enum ProtocolSave {
    Updated(Value),
    Superseded,
    IdentityChanged,
}

pub fn remember_learned_protocol(
    config: &Value,
    candidate: &LearnedProtocol,
    registry: super::RegistrySource,
) -> ConfigResult<ProtocolSave> {
    let Some(provider_index) = config["providers"].as_array().and_then(|providers| {
        providers.iter().position(|provider| {
            provider["id"] == candidate.provider_id && provider["protocol"] == "auto"
        })
    }) else {
        return Ok(ProtocolSave::IdentityChanged);
    };
    let model_index = config["models"].as_array().and_then(|models| {
        models.iter().position(|model| {
            model["id"] == candidate.model_id && model["provider"] == candidate.provider_id
        })
    });
    if candidate.source == RouteSource::ExplicitModel && model_index.is_none() {
        return Ok(ProtocolSave::IdentityChanged);
    }
    let provider = &config["providers"][provider_index];
    let model = model_index.map(|index| &config["models"][index]);
    let unlisted_model =
        Map::from_iter([("id".to_owned(), Value::String(candidate.model_id.clone()))]);
    let key = ProtocolObservationKey::from_model(
        provider.as_object().expect("normalized provider"),
        model.and_then(Value::as_object).unwrap_or(&unlisted_model),
    );
    if key != candidate.key {
        return Ok(ProtocolSave::IdentityChanged);
    }

    let observed_at = parse_observed_at(&candidate.observed_at);
    let observation_time = |observation: &Value| {
        observation["observed_at"]
            .as_str()
            .and_then(parse_observed_at)
    };
    let update_provider = observation_time(&provider["protocol_observation"])
        .is_none_or(|current| Some(current) < observed_at);
    let model_index = model_index.filter(|&index| {
        let observation = &config["models"][index]["protocol_observation"];
        !key.matches_observation(observation) || observation_time(observation) <= observed_at
    });
    if !update_provider && model_index.is_none() {
        return Ok(ProtocolSave::Superseded);
    }

    let observation = key.observation(&candidate.observed_at);
    let protocol = Value::String(candidate.protocol.as_config_str().to_owned());
    let mut updated = config.clone();
    if update_provider {
        let provider = &mut updated["providers"][provider_index];
        provider["resolved_protocol"] = protocol.clone();
        provider["protocol_observation"] = observation.clone();
    }
    if let Some(index) = model_index {
        let model = &mut updated["models"][index];
        model["resolved_protocol"] = protocol;
        model["protocol_observation"] = observation;
    }
    normalize_configuration_with_registry(Some(&updated), registry).map(ProtocolSave::Updated)
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

/// Scheme, lowercase host and effective port of a provider base URL.
type UrlOrigin = (String, String, Option<u16>);

fn base_url_origin(raw: &str) -> Option<UrlOrigin> {
    let parsed = parse_provider_url(raw.trim())?;
    let host = hostname(parsed.netloc)?.to_ascii_lowercase();
    let host_port = parsed
        .netloc
        .rsplit_once('@')
        .map_or(parsed.netloc, |(_, host_port)| host_port);
    let port_text = match host_port.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once(']')?.1.strip_prefix(':'),
        None => host_port.rsplit_once(':').map(|(_, port)| port),
    }
    .filter(|port| !port.is_empty());
    let port = match port_text {
        Some(port) => Some(port.parse::<u16>().ok()?),
        None => match parsed.scheme.as_str() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        },
    };
    Some((parsed.scheme, host, port))
}

/// Stored credentials must not follow an edited provider to a new scheme, host or port;
/// the same request must explicitly supply any credentials for the new origin.
fn provider_origin_changed(incoming: &Map<String, Value>, old: &Value) -> bool {
    let Some(incoming_url) = incoming.get("base_url") else {
        return false;
    };
    let old_url = old.get("base_url").and_then(Value::as_str).unwrap_or("");
    let Some(incoming_url) = incoming_url.as_str() else {
        return true;
    };
    match (base_url_origin(incoming_url), base_url_origin(old_url)) {
        (Some(incoming), Some(old)) => incoming != old,
        _ => incoming_url.trim() != old_url.trim(),
    }
}

fn is_claude_login(provider: &Map<String, Value>) -> bool {
    provider.get("execution_backend").and_then(Value::as_str) == Some("claude_cli")
        && provider.get("auth_mode").and_then(Value::as_str) == Some("claude_login")
}

fn merge_web_update_at_time(
    current: &Value,
    incoming: &Value,
    mut observed_at: impl FnMut() -> String,
    registry: super::RegistrySource,
) -> ConfigResult<Value> {
    let Value::Object(_) = incoming else {
        return Err(ConfigError::new("request body must be an object"));
    };
    let mut merged = incoming.clone();
    let merged_object = merged.as_object_mut().expect("checked object");

    for key in [
        "catalog_show_context",
        "language",
        "auto_enable_on_start",
        "auto_review_fallback",
        "external_collaboration",
        "image_routing",
        "decision_provider",
        "model_groups",
        "aero_profiles",
        "memory_model",
        "memory_extraction_model",
        "memory_extraction_effort",
        "memory_consolidation_effort",
        "background_compaction",
        "ultra_reasoning_efforts",
    ] {
        if !merged_object.contains_key(key)
            && let Some(value) = current.get(key)
        {
            merged_object.insert(key.to_owned(), value.clone());
        }
    }

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
            let origin_changed = old.is_some_and(|old| provider_origin_changed(provider, old));
            let claude_login = is_claude_login(provider);
            let local_login_changed =
                old.is_some_and(|old| old.as_object().is_some_and(is_claude_login) != claude_login);
            let vertex_auth_changed = old.is_some_and(|old| {
                (old.get("vertex").is_some() || provider.get("vertex").is_some())
                    && old.get("auth_mode") != provider.get("auth_mode")
            });
            let credential_changed = origin_changed || local_login_changed || vertex_auth_changed;
            let incoming_file = provider.get("api_key_file").cloned();
            let old_file = old.and_then(|value| value.get("api_key_file"));

            if claude_login {
                let api_key = provider
                    .get("api_key")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !api_key.is_empty() && api_key != MASKED_API_KEY {
                    return Err(ConfigError::new(
                        "Claude CLI local login cannot include an API key",
                    ));
                }
                if incoming_file.as_ref().is_some_and(json_truthy) {
                    return Err(ConfigError::new(
                        "Claude CLI local login cannot include a provider key file",
                    ));
                }
                let incoming_url = provider
                    .get("base_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                let old_url = old
                    .and_then(|value| value.get("base_url"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                let carried_old_url = local_login_changed && incoming_url == old_url;
                if !incoming_url.is_empty() && !carried_old_url {
                    return Err(ConfigError::new(
                        "provider.base_url must be empty for Claude CLI local login",
                    ));
                }
                provider.insert("base_url".to_owned(), Value::String(String::new()));
                provider.insert("api_key".to_owned(), Value::String(String::new()));
                provider.insert("api_key_file".to_owned(), Value::String(String::new()));
            } else {
                let masked = provider
                    .get("api_key")
                    .is_some_and(|value| value == MASKED_API_KEY);
                if !provider.contains_key("api_key") || masked {
                    provider.insert(
                        "api_key".to_owned(),
                        old.filter(|_| !credential_changed)
                            .and_then(|value| value.get("api_key"))
                            .cloned()
                            .unwrap_or_else(|| Value::String(String::new())),
                    );
                }
                if incoming_file.as_ref().is_some_and(json_truthy)
                    && incoming_file.as_ref() != old_file
                {
                    return Err(ConfigError::new("provider.api_key_file is managed by EMP"));
                }
                if credential_changed {
                    provider.insert("api_key_file".to_owned(), Value::String(String::new()));
                } else if old_file.is_some_and(json_truthy) {
                    provider.insert(
                        "api_key_file".to_owned(),
                        old_file.expect("truthy old file").clone(),
                    );
                }
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
            account.remove("workspace_shared");
            account.remove("credential_source");
            if old.and_then(|value| value.get("workspace_shared")) == Some(&Value::Bool(true)) {
                account.insert("workspace_shared".to_owned(), Value::Bool(true));
            }
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
            if let Some(old) = old {
                for field in ["credential_status", "credential_source", "quota"] {
                    if let Some(value) = old.get(field) {
                        account.insert(field.to_owned(), value.clone());
                    }
                }
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
    super::service_names::preserve_service_route_names(current, &mut merged);
    let previous_aliases = super::route_aliases::client_route_aliases(current);
    let next_aliases = super::route_aliases::client_route_aliases(&merged);
    super::aero::preserve_names(current, &mut merged);
    let old_groups = values_by_id(current.get("model_groups"));
    if let Some(groups) = merged["model_groups"].as_array_mut() {
        for group in groups {
            let Some(id) = group["id"].as_str().map(str::to_owned) else {
                continue;
            };
            let Some(old) = old_groups.get(id.as_str()) else {
                continue;
            };
            let mut names = old["route_names"].as_array().cloned().unwrap_or_default();
            for (alias, owner) in &previous_aliases {
                if owner == &id
                    && next_aliases.get(alias) != Some(owner)
                    && !names.iter().any(|name| name == alias)
                {
                    names.push(Value::String(alias.clone()));
                }
            }
            if !names.is_empty() {
                group["route_names"] = Value::Array(names);
            }
        }
    }
    normalize_configuration_with_registry(Some(&merged), registry)
}

/// Apply a Web update preserving secrets and discovery metadata; private paths are not canonicalized.
pub fn merge_web_update(current: &Value, incoming: &Value) -> ConfigResult<Value> {
    merge_web_update_with_registry(current, incoming, super::official_registry::snapshot)
}

pub fn merge_web_update_with_registry(
    current: &Value,
    incoming: &Value,
    registry: super::RegistrySource,
) -> ConfigResult<Value> {
    merge_web_update_at_time(current, incoming, observed_at_now, registry)
}

pub fn merge_web_update_with_time(
    current: &Value,
    incoming: &Value,
    observed_at: &str,
) -> ConfigResult<Value> {
    merge_web_update_at_time(
        current,
        incoming,
        || observed_at.to_owned(),
        super::official_registry::snapshot,
    )
}
