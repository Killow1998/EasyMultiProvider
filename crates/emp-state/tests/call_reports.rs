use emp_state::usage::{
    ledger::{CallFilter, UsageLedger},
    pricing::PriceCatalog,
};
use serde_json::{Value, json};
use std::sync::Arc;
fn filter() -> CallFilter {
    CallFilter {
        start: 0.0,
        end: 1000.0,
        category: None,
        provider: None,
        account: None,
        model: None,
        models: Vec::new(),
        session: None,
        state: None,
        request: None,
        offset: 0,
        limit: 50,
        models_offset: 0,
        models_sort: "calls".into(),
    }
}
#[test]
fn durable_reports_share_accounting_and_keep_delivery_distinct() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let ledger = UsageLedger::new(path.clone(), Arc::clone(&prices));
    let first = json!({"request_id":"one","route":"responses","usage_category":"external","usage_owner":"demo",
        "provider_id":"demo","model_id":"demo/model","upstream_model":"model","client_model":"alias",
        "response_model":"model-revision","model_name_status":"different","thread_id":"thread-one",
        "usage_response_id":"response-one","input_tokens":100,"output_tokens":20,"cached_input_tokens":50,
        "duration_ms":2000,"ttft_ms":100,"performance_schema":4,"tokens_per_second":10.0,
        "success":true,"status":200,"prompt":"PRIVATE","api_key":"PRIVATE","parent_thread_id":{"text":"PRIVATE"}});
    ledger.record(&first, 100.0);
    ledger.record_call(&first, 98.0, 100.0).unwrap();
    ledger.record_call(&first, 98.0, 100.0).unwrap(); // Repeated finish never creates a second call.
    ledger
        .record_call_delivery(
            "one",
            "responses",
            &json!({"delivery":"write_failed","downstream_terminal":"unknown","error_origin":"client","error_code":"write_failed","last_phase":"execute_and_relay","output":"PRIVATE"}),
        )
        .unwrap();
    let mut second = first.clone();
    second["request_id"] = json!("two");
    second["usage_response_id"] = json!("response-two");
    second["thread_id"] = json!("thread-two");
    second["upstream_model"] = json!("other-model");
    second["ttft_ms"] = json!(300);
    second["tokens_per_second"] = json!(30.0);
    ledger.record(&second, 200.0);
    ledger.record_call(&second, 198.0, 200.0).unwrap();
    let mut cancelled = first.clone();
    cancelled["request_id"] = json!("three");
    cancelled["success"] = json!(false);
    cancelled["error_class"] = json!("client_disconnect");
    cancelled["error_origin"] = json!("client");
    cancelled["usage_response_id"] = Value::Null;
    ledger.record_call(&cancelled, 299.0, 300.0).unwrap();
    drop(ledger);
    let ledger = UsageLedger::new(path, prices);
    let report = ledger.query_calls(&filter()).unwrap();
    assert_eq!(report["total"], 3);
    assert_eq!(report["summary"]["completed"], 2);
    assert_eq!(report["summary"]["tokens_per_second"], 20.0);
    assert_eq!(report["summary"]["ttft_ms"], 200.0);
    assert_eq!(report["models_total"], 2);
    assert_eq!(report["records"][0]["state"], "cancelled");
    assert_eq!(report["records"][0]["error_origin"], "client");
    assert!(!report.to_string().contains("PRIVATE"));
    let one = &report["records"][2];
    assert_eq!(one["response_model"], "model-revision");
    assert_eq!(one["delivery"]["delivery"], "write_failed");
    assert_eq!(one["delivery"]["error_origin"], "client");
    assert_eq!(one["delivery"]["error_code"], "write_failed");
    assert_eq!(one["delivery"]["last_phase"], "execute_and_relay");
    assert_eq!(one["state"], "completed");
    assert!(one["pricing"].is_object());
    assert_eq!(
        ledger.query(0.0, 1000.0, "all", 1000.0).unwrap()["totals"]["requests"],
        2
    );
    let mut selected = filter();
    selected.session = Some("thread-one".into());
    assert_eq!(ledger.query_calls(&selected).unwrap()["total"], 2);
    selected = filter();
    selected.models = vec!["absent".into()];
    assert_eq!(ledger.query_calls(&selected).unwrap()["total"], 0);
    selected = filter();
    selected.models_offset = 1;
    assert_eq!(
        ledger.query_calls(&selected).unwrap()["models"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    selected.models_offset = 2;
    assert!(
        ledger.query_calls(&selected).unwrap()["models"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    selected = filter();
    selected.category = Some("native".into());
    assert_eq!(ledger.query_calls(&selected).unwrap()["total"], 0);
    selected = filter();
    selected.offset = 1;
    selected.limit = 1;
    assert_eq!(
        ledger.query_calls(&selected).unwrap()["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
