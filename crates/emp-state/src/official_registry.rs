use emp_domain::json_truthy;
use std::borrow::Cow;
use std::fs::File;
use std::io::Read;
use std::path::Path;
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
const MAX_REGISTRY_BYTES: usize = 4 * 1024 * 1024;

fn bundled_registry() -> &'static Map<String, Value> {
    static REGISTRY: OnceLock<Map<String, Value>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        serde_json::from_str(include_str!("../data/official_models.json"))
            .expect("bundled official model registry must be a JSON object")
    })
}

fn registry_from_path(path: Option<&Path>) -> Cow<'static, Map<String, Value>> {
    let override_data = path.and_then(|path| {
        let mut bytes = Vec::new();
        File::open(path)
            .ok()?
            .take((MAX_REGISTRY_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > MAX_REGISTRY_BYTES {
            return None;
        }
        let data: Map<String, Value> = serde_json::from_slice(&bytes).ok()?;
        (data.get("schema_version").and_then(Value::as_u64) == Some(1)
            && data.get("providers").is_some_and(Value::is_array)
            && data.get("models").is_some_and(Value::is_array)
            && data.get("reviewed_at").is_some_and(Value::is_string))
        .then_some(data)
    });
    override_data
        .map(Cow::Owned)
        .unwrap_or_else(|| Cow::Borrowed(bundled_registry()))
}

fn registry() -> Cow<'static, Map<String, Value>> {
    let path = std::env::var_os("EMP_OFFICIAL_MODELS_PATH").map(std::path::PathBuf::from);
    registry_from_path(path.as_deref())
}

/// Published text models are a reference catalog, not a check of account access.
pub fn published_text_models(provider_key: &str) -> Vec<Value> {
    let registry = registry();
    let Some(publisher) = registry["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["key"] == provider_key)
    else {
        return Vec::new();
    };
    let models = registry
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| {
            model["provider_key"] == provider_key
                && model["output_modalities"]
                    .as_array()
                    .is_some_and(|modes| modes.iter().any(|mode| mode == "text"))
        })
        .filter_map(|model| model["model_id"].as_str())
        .map(|id| json!({"upstream_id":id,"display_name":id}))
        .collect();
    let reference = json!({"base_url": publisher["api_base_urls"][0]});
    enrich_models(reference.as_object().unwrap(), models, false, &registry)
}

pub fn identify_provider(provider: &Map<String, Value>) -> Option<String> {
    let registry = registry();
    identify_provider_in_registry(provider, &registry)
}

fn identify_provider_in_registry(
    provider: &Map<String, Value>,
    registry: &Map<String, Value>,
) -> Option<String> {
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
    enrich_models(provider, models, false, &registry())
}

/// Refresh known modalities without resetting saved limits or reasoning settings.
pub fn enrich_saved_models(provider: &Map<String, Value>, models: Vec<Value>) -> Vec<Value> {
    enrich_models(provider, models, true, &registry())
}

/// Relays inherit published modalities, not the publisher's deployment limits;
/// Claude CLI has its own published reasoning defaults.
fn enrich_models(
    provider: &Map<String, Value>,
    models: Vec<Value>,
    saved: bool,
    registry: &Map<String, Value>,
) -> Vec<Value> {
    let claude_cli =
        provider.get("execution_backend").and_then(Value::as_str) == Some("claude_cli");
    let provider_key = if claude_cli {
        Some("anthropic".to_owned())
    } else {
        identify_provider_in_registry(provider, registry)
    };
    let match_any_publisher =
        provider_key.is_none() || provider_key.as_deref() == Some("openrouter");
    let model_aliases = registry
        .get("providers")
        .and_then(Value::as_array)
        .and_then(|providers| {
            providers
                .iter()
                .find(|record| record.get("key").and_then(Value::as_str) == provider_key.as_deref())
        })
        .and_then(|record| record.get("model_aliases"));
    let modalities_only = match_any_publisher || (saved && !claude_cli);
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
            let Value::Object(mut model) = model else {
                return model;
            };
            let Some(model_id) = ["upstream_id", "model_id", "id"]
                .into_iter()
                .filter_map(|field| model.get(field))
                .find(|value| json_truthy(value))
                .and_then(Value::as_str)
            else {
                return Value::Object(model);
            };
            let registry_id = model_aliases
                .and_then(|aliases| aliases.get(model_id))
                .and_then(Value::as_str)
                .unwrap_or(model_id);
            let Some(official) = registry_models
                .iter()
                .filter_map(Value::as_object)
                .filter(|entry| {
                    match_any_publisher
                        || entry.get("provider_key").and_then(Value::as_str)
                            == provider_key.as_deref()
                })
                .find(|entry| model_matches(entry, registry_id))
            else {
                return Value::Object(model);
            };
            let mut sources = model
                .get("capability_sources")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();

            let mut provenance = provenance.clone();
            if let Some(reviewed_at) = official.get("reviewed_at") {
                provenance["observed_at"] = reviewed_at.clone();
            }
            let vertex_publisher =
                provider.contains_key("vertex") && official["provider_key"] == "google_gemini";
            for field in TOP_LEVEL_FIELDS {
                let publisher_default = vertex_publisher
                    && matches!(
                        field,
                        "context_window"
                            | "max_input_tokens"
                            | "supports_reasoning"
                            | "reasoning_levels"
                            | "reasoning_control"
                    );
                if modalities_only
                    && !publisher_default
                    && !matches!(field, "input_modalities" | "output_modalities")
                {
                    continue;
                }
                if claude_cli
                    && !matches!(
                        field,
                        "input_modalities"
                            | "output_modalities"
                            | "supports_reasoning"
                            | "reasoning_levels"
                            | "reasoning_control"
                    )
                {
                    continue;
                }
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
                if claude_cli
                    || (modalities_only && !(vertex_publisher && target == "output_limit"))
                {
                    continue;
                }
                let official_value = official.get(registry_field).unwrap_or(&Value::Null);
                if is_missing(official_value) {
                    continue;
                }
                let current = if nested {
                    model
                        .get("capabilities")
                        .and_then(|fields| fields.get(target))
                } else {
                    model.get(target)
                };
                if !can_override(current.unwrap_or(&Value::Null), &sources, target) {
                    continue;
                }
                if nested {
                    let capabilities = model.entry("capabilities").or_insert(Value::Null);
                    if !capabilities.is_object() {
                        *capabilities = json!({});
                    }
                    capabilities[target] = official_value.clone();
                } else {
                    model.insert(target.to_owned(), official_value.clone());
                }
                sources.insert(target.to_owned(), provenance.clone());
            }

            if !sources.is_empty() {
                model.insert("capability_sources".to_owned(), Value::Object(sources));
            }
            Value::Object(model)
        })
        .collect()
}

pub fn published_reasoning_levels(publisher: &str, id: &str) -> Option<Value> {
    registry()["models"]
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|model| model["provider_key"] == publisher && model_matches(model, id))?
        .get("reasoning_levels")
        .cloned()
}

/// Claude Code defaults differ from Messages API defaults for some models.
/// Only exact published IDs/aliases are recognized; deployment names are not guessed.
pub fn claude_cli_default_effort(model_id: &str) -> Option<String> {
    let registry = registry();
    registry
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|model| {
            model.get("provider_key").and_then(Value::as_str) == Some("anthropic")
                && model_matches(model, model_id)
        })?
        .get("claude_cli_default_effort")?
        .as_str()
        .map(str::to_owned)
}

fn model_matches(model: &Map<String, Value>, candidate: &str) -> bool {
    let original = candidate;
    let candidate = model
        .get("provider_key")
        .and_then(Value::as_str)
        .and_then(|key| candidate.strip_prefix(key)?.strip_prefix('/'))
        .unwrap_or(candidate);
    model.get("model_id").and_then(Value::as_str) == Some(candidate)
        || model
            .get("aliases")
            .and_then(Value::as_array)
            .is_some_and(|aliases| {
                aliases
                    .iter()
                    .any(|alias| matches!(alias.as_str(), Some(value) if value == candidate || value == original))
            })
}

fn can_override(current: &Value, sources: &Map<String, Value>, field: &str) -> bool {
    let source = sources.get(field).and_then(|value| {
        value.as_str().or_else(|| {
            value
                .as_object()
                .and_then(|value| value.get("source"))
                .and_then(Value::as_str)
        })
    });
    match source {
        Some("manual" | "observed" | "advertised") => false,
        Some("unknown" | "inferred" | "catalog" | "official") => true,
        _ => is_missing(current),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_override_reloads_changes_and_rejects_invalid_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "emp-official-registry-{}-{nonce}.json",
            std::process::id()
        ));
        let provider = json!({"base_url":"https://example.test/v1"});
        let provider = provider.as_object().expect("provider object");
        for reviewed_at in ["first", "second"] {
            let data = json!({
                "schema_version": 1,
                "reviewed_at": reviewed_at,
                "providers": [{"key":"example", "api_base_urls":["https://example.test/v1"]}],
                "models": [],
            });
            std::fs::write(&path, serde_json::to_vec(&data).expect("registry JSON"))
                .expect("write registry override");
            let loaded = registry_from_path(Some(&path));
            assert_eq!(
                loaded.get("reviewed_at").and_then(Value::as_str),
                Some(reviewed_at)
            );
            assert_eq!(
                identify_provider_in_registry(provider, &loaded).as_deref(),
                Some("example")
            );
        }
        std::fs::write(&path, b"invalid JSON").expect("write invalid override");
        assert_eq!(registry_from_path(Some(&path)).as_ref(), bundled_registry());
        std::fs::remove_file(&path).expect("remove registry override");
    }
}
