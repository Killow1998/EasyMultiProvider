use emp_state::{duplicate_account_status, migrate_duplicate_native_visibility, same_account_auth};
use serde_json::{Value, json};

/// A duplicate native login is detected by account_id overlap and its hidden
/// models migrate into the native list exactly once (idempotent on re-run).
#[test]
fn duplicate_visibility_uses_identity_overlap_and_is_idempotent() {
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
    // Untouched configuration sections and independent accounts are preserved.
    assert_eq!(migrated["untouched"], fixture["config"]["untouched"]);
    let hidden: Vec<&Value> = migrated["accounts"]
        .as_array()
        .expect("accounts")
        .iter()
        .map(|account| &account["hidden_models"])
        .collect();
    assert_eq!(hidden[2], &json!(["other-hidden"]));
    assert_eq!(hidden[3], &json!(["leave-other"]));
    assert_eq!(hidden[4], &json!(["invalid-hidden"]));
}
