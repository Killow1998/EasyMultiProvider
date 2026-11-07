use super::auto_review_contract::{make_review_server, write_owned_account_catalog};
use crate::services::{
    request_outcome::RequestOutcome,
    request_preparation::{RequestOperation, prepare_request},
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[test]
fn subscription_balance_keeps_affinity_and_follows_actual_account_after_native_login_changes() {
    let endpoint = "http://127.0.0.1:9/v1";
    let (_directory, server, config) = make_review_server(
        endpoint,
        &[
            json!({"id":"a","prefix":"a","enabled":true}),
            json!({"id":"b","prefix":"b","enabled":true}),
        ],
        Some("regular"),
    );
    let models =
        vec![json!({"slug":"gpt-6-astra","supported_in_api":true,"context_window":128000})];
    std::fs::write(
        config["native_catalog_path"].as_str().unwrap(),
        serde_json::to_vec(&json!({"models":models})).unwrap(),
    )
    .unwrap();
    for account in config["accounts"].as_array().unwrap() {
        write_owned_account_catalog(&server, account, endpoint, models.clone());
    }
    let quota = |used| json!({"plan_type":"pro","rate_limits":{"primary":{"usedPercent":used}}});
    *server.state.backend.accounts.native_quota.lock().unwrap() = Some(quota(80));
    {
        let mut preferences = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        preferences["accounts"][0]["quota"] = quota(10);
        preferences["accounts"][1]["quota"] = quota(50);
        preferences["model_groups"] = json!([
            {"id":"auto/balanced","name":"balanced","policy":"balanced","sources":["gpt-6-astra","a/gpt-6-astra","b/gpt-6-astra"]},
            {"id":"auto/priority","name":"priority","policy":"priority","sources":["gpt-6-astra","a/gpt-6-astra","b/gpt-6-astra"]}
        ]);
    }
    let prepare = |group: &str, thread: &str| {
        prepare_request(
            &server.state,
            json!({"model":group,"input":"public fixture"}),
            RequestOperation::Responses,
            &BTreeMap::from([
                ("session-id".into(), thread.into()),
                ("X-EMP-Caller-ID".into(), "desktop".into()),
            ]),
        )
        .unwrap()
        .route
    };
    assert_eq!(prepare("auto/balanced", "first").provider_id, "a");
    {
        let mut preferences = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        preferences["accounts"][0]["quota"] = quota(90);
        preferences["accounts"][1]["quota"] = quota(5);
    }
    assert_eq!(prepare("auto/balanced", "first").provider_id, "a");
    assert_eq!(prepare("auto/balanced", "new").provider_id, "b");
    // Exhaustion moves the affected conversation, including its next tool result.
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["accounts"][0]["quota"] = quota(100);
    assert_eq!(prepare("auto/balanced", "first").provider_id, "b");

    let first = prepare("auto/priority", "native-thread");
    assert_eq!(first.provider_id, "codex-native");
    let auth_path = &server.state.backend.accounts.native_auth_path;
    let old_native: Value = serde_json::from_slice(&std::fs::read(auth_path).unwrap()).unwrap();
    let a_path = std::path::Path::new(config["accounts"][0]["auth_file"].as_str().unwrap());
    let a_auth = server
        .state
        .backend
        .configuration
        .vault
        .read_encrypted_json(a_path)
        .unwrap();
    emp_state::atomic_write_private_state(auth_path, &serde_json::to_vec(&a_auth).unwrap())
        .unwrap();
    let b_path = std::path::Path::new(config["accounts"][1]["auth_file"].as_str().unwrap());
    server
        .state
        .backend
        .configuration
        .vault
        .write_encrypted_json(b_path, &old_native)
        .unwrap();
    write_owned_account_catalog(&server, &config["accounts"][1], endpoint, models);
    let moved = prepare("auto/priority", "native-thread");
    assert_eq!(moved.provider_id, "b");
    assert_eq!(
        first.model.value()["_emp_aggregate_owner"],
        moved.model.value()["_emp_aggregate_owner"]
    );
    assert_eq!(
        prepare("auto/priority", "new-login-thread").provider_id,
        "codex-native"
    );

    // A known upstream limit cools this actual account, without retrying its output.
    let mut outcome = RequestOutcome::new(
        &server.state,
        &moved,
        &json!({}),
        &BTreeMap::new(),
        None,
        "responses",
    );
    outcome.dispatch();
    outcome.http_status(429);
    outcome.finish();
    drop(outcome);
    assert_eq!(
        prepare("auto/priority", "native-thread").provider_id,
        "codex-native"
    );
    server.shutdown().unwrap();
}
