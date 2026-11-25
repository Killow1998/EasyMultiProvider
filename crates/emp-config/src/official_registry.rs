use emp_domain::official_models::OfficialModels;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, OnceLock};

use serde_json::{Map, Value};

pub type RegistrySource = fn() -> OfficialModels;

const MAX_REGISTRY_BYTES: usize = 4 * 1024 * 1024;

fn bundled_registry() -> &'static Arc<Map<String, Value>> {
    static REGISTRY: OnceLock<Arc<Map<String, Value>>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Arc::new(
            serde_json::from_str(include_str!("../data/official_models.json"))
                .expect("bundled official model registry must be a JSON object"),
        )
    })
}

fn registry_from_path(path: Option<&Path>) -> Arc<Map<String, Value>> {
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
        .map(Arc::new)
        .unwrap_or_else(|| Arc::clone(bundled_registry()))
}

pub fn bundled_snapshot() -> OfficialModels {
    OfficialModels::new(Arc::clone(bundled_registry()))
}

#[cfg(feature = "test-support")]
pub static ENVIRONMENT_SNAPSHOTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn snapshot() -> OfficialModels {
    #[cfg(feature = "test-support")]
    ENVIRONMENT_SNAPSHOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::var_os("EMP_OFFICIAL_MODELS_PATH").map(std::path::PathBuf::from);
    OfficialModels::new(registry_from_path(path.as_deref()))
}

pub fn published_text_models(provider_key: &str) -> Vec<Value> {
    snapshot().published_text_models(provider_key)
}

pub fn identify_provider(provider: &Map<String, Value>) -> Option<String> {
    snapshot().identify_provider(provider)
}

pub fn enrich_discovered_models(provider: &Map<String, Value>, models: Vec<Value>) -> Vec<Value> {
    snapshot().enrich_discovered_models(provider, models)
}

pub fn enrich_saved_models(provider: &Map<String, Value>, models: Vec<Value>) -> Vec<Value> {
    snapshot().enrich_saved_models(provider, models)
}

pub fn published_reasoning_levels(publisher: &str, id: &str) -> Option<Value> {
    snapshot().published_reasoning_levels(publisher, id)
}

pub fn claude_cli_default_effort(model_id: &str) -> Option<String> {
    snapshot().claude_cli_default_effort(model_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registry_override_reloads_changes_and_rejects_invalid_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let path = emp_test_support::fixture_root()
            .expect("resolve test fixture root")
            .join(format!(
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
                OfficialModels::new(loaded.clone())
                    .identify_provider(provider)
                    .as_deref(),
                Some("example")
            );
        }
        std::fs::write(&path, b"invalid JSON").expect("write invalid override");
        assert_eq!(
            registry_from_path(Some(&path)).as_ref(),
            bundled_registry().as_ref()
        );
        std::fs::remove_file(&path).expect("remove registry override");
    }
}
