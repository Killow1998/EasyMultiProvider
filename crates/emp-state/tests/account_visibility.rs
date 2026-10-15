use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use emp_state::{duplicate_account_status, same_account_auth};
use serde_json::json;

/// Only consistent identity can assign a canonical account owner.
#[test]
fn duplicate_accounts_require_consistent_identity() {
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
    assert!(!same_account_auth(
        &fixture["native"],
        &fixture["auths"]["same-token"]
    ));
    assert_eq!(duplicates["same-owner"], "当前 Codex 登录");
    assert!(!duplicates.contains_key("same-token"));
    assert_eq!(duplicates["duplicate-other"], "independent");
    assert_eq!(duplicates.len(), 2);
    let auth = |user: &str, access: &str| {
        json!({"tokens": {
            "account_id":"shared-workspace", "access_token": access,
            "id_token":format!("fixture.{}.signature", URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
                "https://api.openai.com/auth":{"chatgpt_account_id":"shared-workspace","chatgpt_user_id":user}
            })).unwrap()))
        }})
    };
    assert!(same_account_auth(
        &auth("same-user", "before"),
        &auth("same-user", "rotated")
    ));
    assert!(!same_account_auth(
        &auth("first-user", "first"),
        &auth("second-user", "second")
    ));
    assert!(!same_account_auth(
        &auth("first-user", "shared"),
        &auth("second-user", "shared")
    ));
    let workspace = vec![
        ("first".into(), auth("first-user", "first")),
        ("second".into(), auth("second-user", "second")),
    ];
    assert!(duplicate_account_status(None, &workspace).is_empty());
}
