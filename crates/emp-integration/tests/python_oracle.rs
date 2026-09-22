use emp_integration::IntegrationManager;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;

fn normalize_status(status: &emp_integration::IntegrationStatus) -> Value {
    json!({
        "state":status.state,"relation":status.relation,"config_exists":status.config_exists,
        "fields":status.fields,"same_instance":status.same_instance,"conflicts":status.conflicts,
        "lease_status":status.lease.as_ref().map(|lease|lease.status.as_str())
    })
}

#[test]
fn integration_enable_status_and_restore_match_live_python() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let python_root = tempfile::tempdir().unwrap();
    let rust_root = tempfile::tempdir().unwrap();
    let initial = "# header\nopenai_base_url   = \"native\"  # keep\ntitle = \"keep\"\n[nested]\nopenai_base_url = \"nested\"\n";
    fs::write(python_root.path().join("config.toml"), initial).unwrap();
    fs::write(rust_root.path().join("config.toml"), initial).unwrap();
    let script = r#"
import json, sys
from pathlib import Path
from easy_multi_provider.integration import IntegrationManager
root=Path(sys.argv[1]); manager=IntegrationManager(root/'config.toml',root/'state'/'lease.json',instance_id='fixture')
def status(value):
  return {'state':value.state,'relation':value.relation,'config_exists':value.config_exists,
    'fields':{key:item.to_dict() for key,item in value.fields.items()},'same_instance':value.same_instance,
    'conflicts':list(value.conflicts),'lease_status':value.lease.status if value.lease else None}
out=[status(manager.status())]
enabled=manager.enable('http://127.0.0.1:123/v1','catalog.json',True)
out += [{'result':[enabled.action,enabled.state,enabled.relation]}, status(manager.status()), {'config':(root/'config.toml').read_text()}]
restored=manager.restore(); out += [{'result':[restored.action,restored.state,restored.relation]},status(manager.status()),{'config':(root/'config.toml').read_text()}]
print(json.dumps(out,sort_keys=True))
"#;
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(python_root.path())
        .current_dir(source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Value = serde_json::from_slice(&output.stdout).unwrap();
    let manager = IntegrationManager::new(
        rust_root.path().join("config.toml"),
        rust_root.path().join("state/lease.json"),
        Some("fixture".to_owned()),
    )
    .unwrap();
    let mut actual = vec![normalize_status(&manager.status().unwrap())];
    let enabled = manager
        .enable("http://127.0.0.1:123/v1", Some("catalog.json"), true)
        .unwrap();
    actual.push(json!({"result":[enabled.action,enabled.state,enabled.relation]}));
    actual.push(normalize_status(&manager.status().unwrap()));
    actual
        .push(json!({"config":fs::read_to_string(rust_root.path().join("config.toml")).unwrap()}));
    let restored = manager.restore().unwrap();
    actual.push(json!({"result":[restored.action,restored.state,restored.relation]}));
    actual.push(normalize_status(&manager.status().unwrap()));
    actual
        .push(json!({"config":fs::read_to_string(rust_root.path().join("config.toml")).unwrap()}));
    assert_eq!(Value::Array(actual), expected);
}
