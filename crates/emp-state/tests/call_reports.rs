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
fn completed_history_recovery_links_its_rejection_without_double_counting_usage() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let ledger = UsageLedger::new(path.clone(), Arc::clone(&prices));
    for id in ["rejected", "unrelated"] {
        ledger
            .record_call(
                &json!({"request_id":id,"route":"responses","model_id":"demo/model",
            "success":false,"error_code":"previous_response_not_found","status":400,"attempts":0}),
                10.0,
                10.01,
            )
            .unwrap();
    }
    ledger
        .record_call(
            &json!({"request_id":"retried","route":"responses","model_id":"demo/model",
        "success":true,"status":200,"attempts":1,"input_tokens":100,"output_tokens":5,
        "continuation_recovery_from":"rejected"}),
            10.1,
            11.0,
        )
        .unwrap();
    drop(ledger);
    let ledger = UsageLedger::new(path, prices);
    let report = ledger.query_calls(&filter()).unwrap();
    assert_eq!(report["summary"]["completed"], 1);
    assert_eq!(report["summary"]["recovery_required"], 2);
    assert_eq!(report["summary"]["success_samples"], 1);
    assert_eq!(report["summary"]["input_tokens"], 100);
    assert_eq!(report["summary"]["output_tokens"], 5);
    let records = report["records"].as_array().unwrap();
    let rejected = records
        .iter()
        .find(|row| row["request_id"] == "rejected")
        .unwrap();
    assert_eq!(rejected["state"], "recovery_required");
    assert_eq!(rejected["recovery_request_id"], "retried");
    assert_eq!(rejected["attempts"], 0);
    assert!(
        records
            .iter()
            .find(|row| row["request_id"] == "unrelated")
            .unwrap()["recovery_request_id"]
            .is_null()
    );
    assert_eq!(
        records
            .iter()
            .find(|row| row["request_id"] == "retried")
            .unwrap()["continuation_recovery_from"],
        "rejected"
    );
}

#[test]
fn report_progress_counts_filtered_work_and_stops_when_the_reader_leaves() {
    let root = tempfile::tempdir().unwrap();
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let ledger = UsageLedger::new(root.path().join("usage.sqlite3"), prices);
    for index in 0..6 {
        ledger
            .record_call(
                &json!({"request_id":format!("request-{index}"),
            "route":"responses", "model_id":if index % 2 == 0 { "selected" } else { "other" },
            "success":true}),
                10.0,
                11.0,
            )
            .unwrap();
    }
    let mut selected = filter();
    selected.model = Some("selected".into());
    selected.offset = 1;
    selected.limit = 1;
    let expected = ledger.query_calls(&selected).unwrap();
    let updates = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&updates);
    let actual = ledger
        .query_calls_with_progress(
            &selected,
            move |completed, total| {
                sink.lock().unwrap().push((completed, total));
                Ok(())
            },
            None,
        )
        .unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual["total"], 3);
    assert_eq!(actual["records"].as_array().unwrap().len(), 1);
    let updates = updates.lock().unwrap();
    assert_eq!(updates.first(), Some(&(0, 12)));
    assert_eq!(updates.last(), Some(&(12, 12)));
    assert!(
        updates
            .windows(2)
            .all(|pair| pair[0].0 < pair[1].0 && pair[0].1 == pair[1].1)
    );
    assert!(
        ledger
            .query_calls_with_progress(
                &selected,
                |completed, _| {
                    if completed > 0 {
                        Err(std::io::ErrorKind::BrokenPipe.into())
                    } else {
                        Ok(())
                    }
                },
                None
            )
            .is_err()
    );
    assert_eq!(ledger.query_calls(&selected).unwrap(), expected);
    selected.model = Some("missing".into());
    let empty = ledger
        .query_calls_with_progress(
            &selected,
            |completed, total| {
                assert_eq!((completed, total), (0, 0));
                Ok(())
            },
            None,
        )
        .unwrap();
    assert_eq!(empty["total"], 0);
}

#[test]
fn reported_tiers_and_effort_survive_restart_without_relabeling_legacy_requests() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let ledger = UsageLedger::new(path.clone(), Arc::clone(&prices));
    for (id, response, expected) in [
        ("standard", json!({"service_tier":"default"}), "standard"),
        ("fast", json!({"service_tier":"priority"}), "fast"),
        (
            "ultrafast",
            json!({"service_tier":"ultrafast"}),
            "ultrafast",
        ),
        ("flex", json!({"service_tier":"flex"}), "flex"),
        ("unconfirmed", json!({}), "unknown"),
    ] {
        let mut event = json!({"request_id":id,"route":"responses","model_id":"demo/model",
            "requested_service_tier":"priority","reported_service_tier":null,"service_tier":"priority",
            "requested_effort":"ultra","reported_effort":"xhigh","speed_mode":"unknown","success":true});
        event
            .as_object_mut()
            .unwrap()
            .extend(emp_state::usage::reported_usage(
                &json!({"type":"response.completed","response":response}),
            ));
        assert_eq!(event["speed_mode"], expected);
        ledger.record_call(&event, 10.0, 11.0).unwrap();
    }
    ledger
        .record_call(
            &json!({"request_id":"legacy","route":"responses","model_id":"demo/model",
        "service_tier":"default","speed_mode":"fast","success":true}),
            10.0,
            11.0,
        )
        .unwrap();
    drop(ledger);
    let ledger = UsageLedger::new(path.clone(), prices);
    let report = ledger.query_calls(&filter()).unwrap();
    let records = report["records"].as_array().unwrap();
    for row in records {
        if row["request_id"] == "legacy" {
            assert_eq!(row["speed_mode"], "unknown");
            assert!(row["requested_service_tier"].is_null());
        } else {
            assert_eq!(row["requested_service_tier"], "priority");
            assert_eq!(row["requested_effort"], "ultra");
            assert_eq!(row["reported_effort"], "xhigh");
        }
    }
    assert_eq!(report["models"].as_array().unwrap().len(), 6);
    let connection = rusqlite::Connection::open(path).unwrap();
    let saved: String = connection
        .query_row(
            "SELECT json_extract(facts,'$.speed_mode') FROM call_records WHERE request_id='legacy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(saved, "fast");
}

#[test]
fn legacy_ws_errors_and_recovery_are_not_client_interruptions() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let ledger = UsageLedger::new(
        path.clone(),
        Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0)),
    );
    for (id, extra) in [
        ("complete", json!({"success":true,"status":200})),
        ("disconnect", json!({"error_class":"client_disconnect"})),
        ("interrupt", json!({"error_class":"client_cancelled"})),
        ("legacy_failure", json!({})),
        ("legacy_recovery", json!({})),
        ("unknown", json!({})),
    ] {
        let mut event = json!({"request_id":id,"route":"responses","model_id":"demo/model"});
        event
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        ledger.record_call(&event, 10.0, 11.0).unwrap();
    }
    // Simulate the old classifier. Projection must use actual delivery evidence,
    // not mutate old rows or infer success from a nearby unrelated request.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE call_records SET state='interrupted' WHERE request_id LIKE 'legacy_%'",
            [],
        )
        .unwrap();
    ledger.record_call_delivery("legacy_failure","responses",&json!({"downstream_terminal":"failed","error_origin":"transport","error_code":"upstream_error"})).unwrap();
    ledger.record_call_delivery("legacy_recovery","responses",&json!({"downstream_terminal":"error","error_origin":"emp","error_code":"previous_response_not_found"})).unwrap();
    let report = ledger.query_calls(&filter()).unwrap();
    for key in [
        "completed",
        "failed",
        "interrupted",
        "cancelled",
        "recovery_required",
        "unknown",
    ] {
        assert_eq!(report["summary"][key], 1, "{key}: {report}");
    }
    assert_eq!(report["summary"]["success_samples"], 2);
    assert_eq!(
        report["summary"]["failure_breakdown"],
        json!([
            {"origin":"transport","code":"upstream_error","status":null,"count":1}
        ])
    );
    let mut paged = filter();
    paged.limit = 1;
    paged.offset = 5;
    assert_eq!(
        ledger.query_calls(&paged).unwrap()["summary"]["failure_breakdown"],
        report["summary"]["failure_breakdown"]
    );
    let mut completed = filter();
    completed.state = Some("completed".into());
    assert_eq!(
        ledger.query_calls(&completed).unwrap()["summary"]["failure_breakdown"],
        json!([])
    );
    for expected in ["failed", "recovery_required", "unknown"] {
        let mut selected = filter();
        selected.state = Some(expected.into());
        let subset = ledger.query_calls(&selected).unwrap();
        assert_eq!(subset["total"], 1);
        assert_eq!(subset["records"][0]["state"], expected);
    }
    let unchanged: String = connection
        .query_row(
            "SELECT state FROM call_records WHERE request_id='legacy_failure'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unchanged, "interrupted");
}
#[test]
fn legacy_rates_remain_in_call_details_without_polluting_generation_averages() {
    let root = tempfile::tempdir().unwrap();
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let ledger = UsageLedger::new(root.path().join("usage.sqlite3"), prices);
    ledger
        .record_call(
            &json!({"request_id":"legacy","route":"responses",
        "model_id":"model","success":true,"performance_schema":4,
        "ttft_ms":10,"tokens_per_second":25.0}),
            98.0,
            100.0,
        )
        .unwrap();
    let report = ledger.query_calls(&filter()).unwrap();
    assert_eq!(report["summary"]["completed"], 1);
    assert_eq!(report["summary"]["tokens_per_second"], Value::Null);
    assert_eq!(report["summary"]["tps_samples"], 0);
    assert_eq!(report["summary"]["ttft_ms"], Value::Null);
    assert_eq!(report["records"][0]["tokens_per_second"], 25.0);
    ledger
        .record_call(
            &json!({"request_id":"burst","route":"responses","model_id":"model",
        "success":true,"performance_schema":5,"first_content_ms":11916,"duration_ms":11966,
        "output_tokens":100,"reasoning_tokens":31,"tokens_per_second":8.36,
        "generation_ms":6,"generation_tokens_per_second":11500.0}),
            98.0,
            100.0,
        )
        .unwrap();
    let report = ledger.query_calls(&filter()).unwrap();
    assert_eq!(report["summary"]["tps_samples"], 0);
    assert!(report["summary"]["tokens_per_second"].is_null());
    assert!(report["models"][0]["tokens_per_second"].is_null());
    assert!(report["periods"][0]["tokens_per_second"].is_null());
    let burst = report["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["request_id"] == "burst")
        .unwrap();
    assert_eq!(burst["tokens_per_second"], 8.36);
    assert!(burst["generation_tokens_per_second"].is_null());
    let db = rusqlite::Connection::open(root.path().join("usage.sqlite3")).unwrap();
    let saved:f64=db.query_row("SELECT json_extract(facts,'$.generation_tokens_per_second') FROM call_records WHERE request_id='burst'",[],|row|row.get(0)).unwrap();
    assert_eq!(saved, 11500.0);
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
        "duration_ms":2000,"ttft_ms":10,"first_content_ms":100,"performance_schema":5,
        "tokens_per_second":1.0,"generation_tokens_per_second":10.0,"generation_ms":1000,
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
    second["first_content_ms"] = json!(300);
    second["generation_tokens_per_second"] = json!(30.0);
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
    assert_eq!(report["summary"]["tps_samples"], 2);
    assert_eq!(report["models"][0]["tokens_per_second"], 10.0);
    assert_eq!(report["periods"][0]["tokens_per_second"], 20.0);
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
    assert_eq!(one["tokens_per_second"], 1.0);
    assert_eq!(one["generation_tokens_per_second"], 10.0);
    assert!(one["pricing"].is_object());
    assert_eq!(
        ledger.query(0.0, 1000.0, "all", 1000.0, None).unwrap()["totals"]["requests"],
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
