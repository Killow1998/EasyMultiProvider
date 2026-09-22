use emp_router::official_registry::{enrich_discovered_models, identify_provider};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Stdio};

fn fixture() -> Value {
    json!({
        "provider": {"base_url": "https://api.openai.com/v1/"},
        "models": [
            {"upstream_id": "gpt-5.6-sol"},
            {
                "upstream_id": "gpt-5.6",
                "input_modalities": ["text"],
                "capabilities": {"streaming": false},
                "capability_sources": {
                    "input_modalities": {"source": "advertised"},
                    "streaming": {"source": "unknown"}
                }
            },
            {
                "upstream_id": "gpt-5.6-sol",
                "context_window": 1,
                "capability_sources": {
                    "context_window": {
                        "source": "official",
                        "confidence": 0.95,
                        "observed_at": "2026-01-01"
                    }
                }
            },
            {"upstream_id": "future-model", "context_window": null}
        ],
        "providers": [
            {"base_url": "https://api.openai.com/v1/"},
            {"base_url": "https://user:secret@api.openai.com/v1"},
            {"base_url": "https://api.openai.com/v1?version=1"},
            {
                "base_url": "https://api.openai.com/v1",
                "official_provider": "openai"
            },
            {
                "base_url": "https://proxy.example/v1",
                "official_provider": "openai"
            },
            {"base_url": "https://open.bigmodel.cn/api/paas/v4"}
        ]
    })
}

fn python_oracle(python: &str, fixture: &Value) -> Value {
    let script = r#"
import json
import sys
from easy_multi_provider.official_registry import enrich_discovered_models, identify_provider

fixture = json.load(sys.stdin)
result = {
    "models": enrich_discovered_models(fixture["provider"], fixture["models"]),
    "providers": [identify_provider(provider) for provider in fixture["providers"]],
}
json.dump(result, sys.stdout, ensure_ascii=False, separators=(",", ":"))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut child = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn Python official registry oracle");
    serde_json::to_writer(child.stdin.as_mut().expect("oracle stdin"), fixture)
        .expect("write official registry fixture");
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait for Python oracle");
    assert!(
        output.status.success(),
        "Python official registry oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python official registry JSON")
}

#[test]
fn bundled_registry_enrichment_matches_live_python() {
    let fixture = fixture();
    let provider = fixture["provider"].as_object().expect("provider object");
    let models = fixture["models"].as_array().expect("models").clone();
    let providers = fixture["providers"]
        .as_array()
        .expect("providers")
        .iter()
        .map(|provider| {
            identify_provider(provider.as_object().expect("provider object"))
                .map(Value::String)
                .unwrap_or(Value::Null)
        })
        .collect::<Vec<_>>();
    let rust = json!({
        "models": enrich_discovered_models(provider, models),
        "providers": providers,
    });
    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        assert_eq!(rust, python_oracle(&python, &fixture));
    }
    assert_eq!(rust["providers"][0], "openai");
    assert_eq!(rust["providers"][1], Value::Null);
    assert_eq!(rust["providers"][2], Value::Null);
    assert_eq!(rust["providers"][5], "zhipu_glm");
    assert_eq!(rust["models"][0]["context_window"], 1_050_000);
    assert_eq!(rust["models"][1]["input_modalities"], json!(["text"]));
    assert_eq!(rust["models"][1]["capabilities"]["streaming"], true);
    assert_eq!(rust["models"][2]["context_window"], 1_050_000);
    assert_eq!(rust["models"][3]["context_window"], Value::Null);
}
