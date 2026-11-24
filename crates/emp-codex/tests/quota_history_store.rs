use emp_storage::quota_history::QuotaHistoryStore;
use serde_json::json;

#[test]
fn failed_observations_explain_gaps_without_creating_quota_points() {
    let root = emp_test_support::fixture_dir().unwrap();
    let store = QuotaHistoryStore::new(root.path().join("history.sqlite3"));
    let error = emp_codex::quota::quota_rpc_error(
        "account/rateLimits/read",
        &json!({"message":"error sending request: dns error https://private.invalid/?token=PRIVATE"}),
    );
    let diagnostics = error.diagnostics();
    for at in [1000, 1044, 1088] {
        store.append_failure("account", &diagnostics, at).unwrap();
    }
    let history = store.query_period("account", 900, 1200).unwrap();
    assert_eq!(history["series"], json!([]));
    assert_eq!(history["failures"][0]["count"], 3);
    let saved = &history["failures"][0]["diagnostics"];
    assert_eq!(saved["code"], "quota_transport_error");
    assert_eq!(saved["rpc_method"], "account/rateLimits/read");
    assert_eq!(saved["transport_cause"], "dns");
    let rendered = history.to_string();
    assert!(!rendered.contains("PRIVATE"));
    assert!(!rendered.contains("private.invalid"));
    assert_eq!(
        store.query_period("other", 900, 1200).unwrap()["failures"],
        json!([])
    );
    store.adopt_legacy_key("account", "owner").unwrap();
    assert_eq!(
        store.query_period("owner", 900, 1200).unwrap()["failures"][0]["count"],
        3
    );
    store
        .append_snapshot(
            "owner",
            &json!({"rate_limits":{"primary":{"usedPercent":10,"windowDurationMins":300}}}),
            1088,
        )
        .unwrap();
    let recovered = store.query_period("owner", 900, 1200).unwrap();
    assert_eq!(recovered["failures"][0]["count"], 2);
    assert_eq!(
        recovered["series"][0]["points"].as_array().unwrap().len(),
        1
    );
    store.delete_account("owner").unwrap();
    assert_eq!(
        store.query_period("owner", 900, 1200).unwrap()["failures"],
        json!([])
    );
}
