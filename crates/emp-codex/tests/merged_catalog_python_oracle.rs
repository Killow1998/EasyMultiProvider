use emp_codex::merged_catalog::build_catalog;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

#[test]
fn merged_catalog_replays_existing_python_catalog_tests() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let script = r#"
import copy, importlib, io, json, sys, unittest
from easy_multi_provider import catalog
original = catalog.build_catalog
cases = []
def observed(config):
    native = catalog.load_native_catalog(config)
    duplicates = catalog.duplicate_account_status(config.get('accounts', []))
    accounts = {account['id']: catalog._account_catalog(config, account) for account in config.get('accounts', [])}
    result = original(config)
    cases.append(copy.deepcopy({'config': config, 'native': native, 'accounts': accounts, 'duplicates': duplicates, 'expected': result}))
    return result
catalog.build_catalog = observed
suite = unittest.defaultTestLoader.loadTestsFromModule(importlib.import_module('tests.test_catalog'))
stream = io.StringIO()
result = unittest.TextTestRunner(stream=stream).run(suite)
if not result.wasSuccessful():
    sys.stderr.write(stream.getvalue())
    raise SystemExit(1)
json.dump(cases, sys.stdout, ensure_ascii=False)
"#;
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
        .output()
        .expect("live Python catalog tests");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("catalog fixtures");
    assert!(
        cases.len() >= 20,
        "existing catalog suite must exercise composition"
    );
    for (index, case) in cases.iter().enumerate() {
        let accounts: BTreeMap<String, Value> =
            serde_json::from_value(case["accounts"].clone()).expect("account catalogs");
        let duplicates: BTreeMap<String, String> =
            serde_json::from_value(case["duplicates"].clone()).expect("duplicate sources");
        let actual = build_catalog(&case["config"], &case["native"], &accounts, &duplicates);
        assert_eq!(
            actual, case["expected"],
            "existing Python catalog fixture {index}"
        );
    }
}

#[test]
fn external_catalog_keeps_coding_template_without_native_entitlements() {
    let config = json!({
        "providers":[{"id":"demo","protocol":"chat_completions"}],
        "models":[{"id":"demo/model","provider":"demo","upstream_id":"model","context_window":256000,"reasoning_levels":[]}]
    });
    let native = json!({"models":[{
        "slug":"native", "base_instructions":"coding", "model_messages":{"tools":"safe","unknown_future":"private"},
        "available_access_programs":["native entitlement"],"supports_reasoning_summary_parameter":true,
        "multi_agent_version":"orchestrator"
    }]});
    let catalog = build_catalog(&config, &native, &BTreeMap::new(), &BTreeMap::new());
    let external = catalog["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|entry| entry["slug"] == "demo/model")
        .expect("external");
    assert_eq!(external["base_instructions"], "coding");
    assert_eq!(external["display_name"], "[ 256K]  demo/model");
    assert!(external.get("available_access_programs").is_none());
    assert_eq!(external["multi_agent_version"], Value::Null);
    assert_eq!(external["model_messages"], json!({"tools":"safe"}));
    assert_eq!(external["supported_reasoning_levels"], json!([]));
}
