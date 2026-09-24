use emp_integration::IntegrationManager;
use serde_json::{Value, json};
use std::fs;
use std::process::Command;

fn live_python_oracle() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let python = std::env::var_os("EMP_PYTHON_INTEROP")?;
    let root = std::env::var_os("EMP_PYTHON_ORACLE_ROOT")
        .expect("EMP_PYTHON_ORACLE_ROOT must name the official Python oracle checkout");
    let root = std::path::PathBuf::from(root);
    assert!(root.join("easy_multi_provider/integration.py").is_file());
    Some((python.into(), root))
}

fn normalize_status(status: &emp_integration::IntegrationStatus) -> Value {
    json!({
        "state":status.state,"relation":status.relation,"config_exists":status.config_exists,
        "fields":status.fields,"same_instance":status.same_instance,"conflicts":status.conflicts,
        "lease_status":status.lease.as_ref().map(|lease|lease.status.as_str())
    })
}

#[test]
fn integration_enable_status_and_restore_match_live_python() {
    let Some((python, oracle_root)) = live_python_oracle() else {
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
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(python_root.path())
        .current_dir(oracle_root)
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

#[test]
fn realtime_sideband_and_legacy_v2_restore_match_live_python() {
    let Some((python, oracle_root)) = live_python_oracle() else {
        return;
    };
    let python_root = tempfile::tempdir().unwrap();
    let rust_root = tempfile::tempdir().unwrap();
    let initial = "# keep\nopenai_base_url = \"native\"\nexperimental_realtime_ws_base_url = \"https://voice.example/v1\"\ntitle = \"keep\"\n";
    fs::write(python_root.path().join("config.toml"), initial).unwrap();
    fs::write(rust_root.path().join("config.toml"), initial).unwrap();
    let script = r#"
import json, sys
from pathlib import Path
from easy_multi_provider.integration import IntegrationManager, REALTIME_SIDEBAND_FIELD
root=Path(sys.argv[1]); config=root/'config.toml'; lease_path=root/'state'/'lease.json'
manager=IntegrationManager(config,lease_path,instance_id='fixture')
base='http://127.0.0.1:123/v1'
def status(value):
  return {'state':value.state,'relation':value.relation,'config_exists':value.config_exists,
    'fields':{key:item.to_dict() for key,item in value.fields.items()},
    'same_instance':value.same_instance,'conflicts':list(value.conflicts),
    'lease_status':value.lease.status if value.lease else None}
out=[]
enabled=manager.enable(base,None,True,realtime_sideband_base_url=base)
out += [{'result':[enabled.action,enabled.state,enabled.relation]},status(manager.status()),
        {'config':config.read_text()}]
restored=manager.restore()
out += [{'result':[restored.action,restored.state,restored.relation]},status(manager.status()),
        {'config':config.read_text()}]
# A v2 lease did not own Voice. The current sideband value is both the
# synthetic original and applied value, so restore must preserve it.
manager.enable(base,None,True)
legacy=json.loads(lease_path.read_text())
legacy['version']=2; legacy['fields'].pop(REALTIME_SIDEBAND_FIELD)
lease_path.write_text(json.dumps(legacy))
out += [status(manager.status())]
restored=manager.restore()
record=json.loads(lease_path.read_text())
out += [{'result':[restored.action,restored.state,restored.relation]},
        {'config':config.read_text()},
        {'lease_version':record['version'],'lease_fields':sorted(record['fields'])}]
print(json.dumps(out,sort_keys=True))
"#;
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(python_root.path())
        .current_dir(oracle_root)
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
    let base = "http://127.0.0.1:123/v1";
    let mut actual = Vec::new();
    let enabled = manager
        .enable_with_sideband(base, None, true, Some(base))
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

    manager.enable(base, None, true).unwrap();
    let lease_path = rust_root.path().join("state/lease.json");
    let mut legacy: Value = serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
    legacy["version"] = json!(2);
    legacy["fields"]
        .as_object_mut()
        .unwrap()
        .remove(emp_integration::REALTIME_SIDEBAND_FIELD);
    fs::write(&lease_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    actual.push(normalize_status(&manager.status().unwrap()));
    let restored = manager.restore().unwrap();
    actual.push(json!({"result":[restored.action,restored.state,restored.relation]}));
    actual
        .push(json!({"config":fs::read_to_string(rust_root.path().join("config.toml")).unwrap()}));
    let record: Value = serde_json::from_slice(&fs::read(&lease_path).unwrap()).unwrap();
    let mut lease_fields = record["fields"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    lease_fields.sort();
    actual.push(json!({"lease_version":record["version"],
        "lease_fields":lease_fields }));
    assert_eq!(Value::Array(actual), expected);
}
