use emp_state::{duplicate_account_status, migrate_duplicate_native_visibility, same_account_auth};
use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Stdio};

#[test]
fn duplicate_visibility_uses_python_identity_overlap_and_is_idempotent() {
    let fixture = json!({
        "native":{"tokens":{"access_token":"native-token","account_id":"native-owner"}},
        "auths":{
            "same-owner":{"tokens":{"access_token":"rotated-token","account_id":"native-owner"}},
            "same-token":{"tokens":{"access_token":"native-token","account_id":"different-owner"}},
            "independent":{"tokens":{"access_token":"other-token","account_id":"other-owner"}},
            "duplicate-other":{"tokens":{"access_token":"new-other-token","account_id":"other-owner"}},
            "invalid":{"tokens":{"access_token":"","account_id":"native-owner"}}
        },
        "config":{"native_hidden_models":["already"],"untouched":{"enabled":true},"accounts":[
            {"id":"same-owner","prefix":"same-owner","auth_file":"a.enc","hidden_models":["already","native-a"]},
            {"id":"same-token","prefix":"same-token","auth_file":"b.enc","hidden_models":["native-b","native-a"],"enabled":false},
            {"id":"independent","prefix":"independent","auth_file":"c.enc","hidden_models":["other-hidden"]},
            {"id":"duplicate-other","prefix":"duplicate-other","auth_file":"d.enc","hidden_models":["leave-other"]},
            {"id":"invalid","prefix":"invalid","auth_file":"e.enc","hidden_models":["invalid-hidden"]}
        ]}
    });
    let accounts = fixture["config"]["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|account| {
            let id = account["id"].as_str().expect("id");
            (id.to_owned(), fixture["auths"][id].clone())
        })
        .collect::<Vec<_>>();
    let duplicates = duplicate_account_status(Some(&fixture["native"]), &accounts);
    let (migrated, changed) = migrate_duplicate_native_visibility(&fixture["config"], &duplicates);
    let (again, second_changed) = migrate_duplicate_native_visibility(&migrated, &duplicates);
    assert!(changed);
    assert!(!second_changed);
    assert_eq!(again, migrated);
    assert!(!same_account_auth(
        &fixture["native"],
        &fixture["auths"]["same-token"]
    ));
    assert_eq!(duplicates["same-token"], "当前 Codex 登录");
    assert_eq!(
        migrated["native_hidden_models"],
        json!(["already", "native-a", "native-b"])
    );
    if let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") {
        let mut child=Command::new(python).args(["-c",r#"
import json,sys
from unittest.mock import patch
from easy_multi_provider import accounts
fixture=json.load(sys.stdin)
with patch.object(accounts,'_auth_file_identities',return_value=accounts._auth_identities(accounts._validate_auth(fixture['native']))), \
     patch.object(accounts,'load_auth',side_effect=lambda account:accounts._validate_auth(fixture['auths'][account['id']])):
    duplicates=accounts.duplicate_account_status(fixture['config']['accounts'])
    migrated,changed=accounts.migrate_duplicate_native_visibility(fixture['config'],duplicates)
    again,second_changed=accounts.migrate_duplicate_native_visibility(migrated,duplicates)
json.dump({'duplicates':duplicates,'migrated':migrated,'changed':changed,'again':again,'second_changed':second_changed},sys.stdout)
"#]).current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().expect("Python visibility oracle");
        serde_json::to_writer(child.stdin.take().expect("stdin"), &fixture).expect("fixture");
        let output = child.wait_with_output().expect("oracle output");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected: Value = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        assert_eq!(
            json!({"duplicates":duplicates,"migrated":migrated,"changed":changed,"again":again,"second_changed":second_changed}),
            expected
        );
    }
}
