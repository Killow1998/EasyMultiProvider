use emp_state::usage::{ledger::UsageLedger, pricing::PriceCatalog};
use serde_json::json;
use std::sync::Arc;

#[test]
fn lifetime_pricing_distinguishes_unknown_placeholders_from_known_zero_after_reopen() {
    let root = tempfile::tempdir().unwrap();
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    prices.replace(
        json!({
            "free-model":{"input_cost_per_token":"0","output_cost_per_token":"0"},
            "paid-model":{"input_cost_per_token":"0.001","output_cost_per_token":"0.002"}
        }),
        1.0,
    );
    let path = root.path().join("usage.sqlite3");
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    for model in ["unknown-model", "free-model", "paid-model"] {
        ledger.record(
            &json!({"route":"responses","usage_category":"external", "usage_owner":"api",
            "upstream_model":model,"input_tokens":100,"output_tokens":10}),
            10.0,
        );
    }
    drop(ledger);
    let ledger = UsageLedger::new(path, prices);
    let report = ledger
        .query_lifetime_with_progress("api", 1000.0, |_, _| Ok(()), None)
        .unwrap();
    let groups = report["groups"].as_array().unwrap();
    for (name, priced) in [("unknown-model", 0), ("free-model", 1), ("paid-model", 1)] {
        let row = groups.iter().find(|row| row["model"] == name).unwrap();
        assert_eq!(row["priced_requests"], priced);
        assert_eq!(row["unpriced_requests"], 1 - priced);
    }
    assert_eq!(
        groups
            .iter()
            .find(|row| row["model"] == "free-model")
            .unwrap()["cost_nanos"],
        0
    );
    assert!(
        groups
            .iter()
            .find(|row| row["model"] == "paid-model")
            .unwrap()["cost_nanos"]
            .as_i64()
            .unwrap()
            > 0
    );
}

#[test]
fn memory_receipts_restore_priced_usage_once_and_reconcile_rollout_history() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    prices.replace(
        json!({"example-model":{
            "input_cost_per_token":"0.001", "output_cost_per_token":"0.002",
            "cache_read_input_token_cost":"0.0001"
        }}),
        1.0,
    );
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    let mut events = Vec::new();
    for operation in ["memory_extraction", "memory_consolidation", "responses"] {
        let event = json!({
            "request_id":operation, "route":operation, "usage_response_id":operation,
            "usage_category":"native", "usage_owner":"account:memory",
            "model_id":"example-model", "upstream_model":"example-model",
            "thread_id":"memory-job", "turn_id":operation,
            "success":true, "status":200, "response_status":"completed",
            "input_tokens":1000, "cached_input_tokens":800,
            "output_tokens":20, "reasoning_tokens":5
        });
        ledger.record_call(&event, 10.0, 12.0).unwrap();
        events.push(event);
    }
    assert_eq!(
        ledger.query(0.0, 100.0, "all", 1000.0, None).unwrap()["totals"]["requests"],
        0
    );
    drop(ledger);
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    let report = ledger
        .query_owner(0.0, 100.0, "account:memory", 1000.0, None)
        .unwrap();
    assert_eq!(report["totals"]["requests"], 2);
    assert_eq!(report["totals"]["input_tokens"], 2000);
    assert_eq!(report["totals"]["cached_input_tokens"], 1600);
    assert_eq!(report["totals"]["output_tokens"], 40);
    assert_eq!(report["totals"]["reasoning_tokens"], 10);
    assert_eq!(report["totals"]["cost_nanos"], 640_000_000);
    for event in &events[..2] {
        ledger.record(event, 12.0);
    }
    let mut historical = events[0].clone();
    historical["usage_turn"] = historical["turn_id"].clone();
    historical["route_model"] = historical["model_id"].clone();
    ledger
        .record_history(&[(historical, 12.0)], "memory-rollout", &json!({}))
        .unwrap();
    drop(ledger);
    let reopened = UsageLedger::new(path.clone(), prices.clone());
    let again = reopened
        .query_owner(0.0, 100.0, "account:memory", 1000.0, None)
        .unwrap();
    assert_eq!(again["totals"], report["totals"]);

    let failed = json!({
        "request_id":"failed-memory", "route":"memory_consolidation",
        "usage_category":"native", "usage_owner":"account:memory",
        "model_id":"example-model", "upstream_model":"example-model",
        "success":false, "status":502
    });
    reopened.record(&failed, 14.0);
    reopened.record_call(&failed, 13.0, 14.0).unwrap();
    drop(reopened);
    let final_report = UsageLedger::new(path, prices)
        .query_owner(0.0, 100.0, "account:memory", 1000.0, None)
        .unwrap();
    assert_eq!(final_report["totals"]["requests"], 3);
    assert_eq!(final_report["totals"]["reported_requests"], 2);
    assert_eq!(
        final_report["totals"]["cost_nanos"],
        report["totals"]["cost_nanos"]
    );
}

#[test]
fn account_details_keep_tokens_periods_and_start_time_with_the_verified_owner() {
    let directory = tempfile::tempdir().unwrap();
    let ledger = UsageLedger::new(
        directory.path().join("usage.sqlite3"),
        Arc::new(PriceCatalog::new(
            directory.path().join("prices.json"),
            1000.0,
        )),
    );
    for (owner, category, tokens, observed) in [
        ("account:other", "native", 999, 10.0),
        ("account:selected", "subscription", 100, 120.0),
        ("account:selected", "native", 200, 180.0),
    ] {
        ledger.record(
            &json!({"route":"responses", "usage_category":category,
            "usage_owner":owner, "upstream_model":"example-model", "input_tokens":tokens,
            "output_tokens":10}),
            observed,
        );
    }
    let selected = ledger
        .query_owner(0.0, 1000.0, "account:selected", 1000.0, None)
        .unwrap();
    assert_eq!(selected["totals"]["requests"], 2);
    assert_eq!(selected["totals"]["input_tokens"], 300);
    assert_eq!(selected["totals"]["output_tokens"], 20);
    assert_eq!(selected["first_record_at"], 120.0);
    assert!(
        selected["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["owner"] == "account:selected")
    );
    assert_eq!(selected["periods"].as_array().unwrap().len(), 2);
    assert_eq!(selected["issues"][0]["price_issue"], "unknown_model");
    assert_eq!(selected["totals"]["priced_requests"], 0);
    assert_eq!(selected["totals"]["unpriced_requests"], 2);
    assert!(
        selected["periods"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["priced_requests"] == 0)
    );
    assert_eq!(
        ledger.query(0.0, 1000.0, "all", 1000.0, None).unwrap()["totals"]["requests"],
        3
    );
    let observations = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed = observations.clone();
    let streamed = ledger
        .query_lifetime_with_progress(
            "account:selected",
            1000.0,
            move |completed, total| {
                observed.lock().unwrap().push((completed, total));
                Ok(())
            },
            None,
        )
        .unwrap();
    assert!(
        streamed["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["priced_requests"] == 0)
    );
    let mut groups = streamed["groups"].clone();
    for group in groups.as_array_mut().unwrap() {
        group.as_object_mut().unwrap().remove("read_progress");
    }
    let baseline = ledger
        .query_owner(0.0, 1000.0, "account:selected", 1000.0, None)
        .unwrap();
    assert_eq!(groups, baseline["groups"]);
    assert_eq!(streamed["first_record_at"], baseline["first_record_at"]);
    let observed = observations.lock().unwrap();
    assert_eq!(observed.first().unwrap().0, 0);
    let (completed, total) = observed.last().unwrap();
    assert_eq!(completed, total);
    assert!(*total > 0);
    assert!(
        ledger
            .query_lifetime_with_progress(
                "account:selected",
                1000.0,
                |_, _| Err(std::io::ErrorKind::BrokenPipe.into()),
                None
            )
            .is_err()
    );
    assert!(ledger.query_owner(0.0, 1000.0, "", 1000.0, None).is_err());
}

#[test]
fn chart_series_preserve_totals_and_filter_by_receipt_and_service() {
    use emp_state::usage::ledger::UsageSeriesFilter;
    let root = tempfile::tempdir().unwrap();
    let ledger = UsageLedger::new(
        root.path().join("usage.sqlite3"),
        Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0)),
    );
    for (id, owner, tokens, stamp) in [
        ("a", "first", 10, 10.0),
        ("b", "first", 20, 70.0),
        ("c", "second", 30, 90.0),
    ] {
        let event = json!({"request_id":id,"observation_id":id,"route_model":"route/model","route":"responses","usage_category":"external",
            "usage_owner":owner,"provider_id":owner,"upstream_model":"model","model_id":"route/model",
            "thread_id":id,"input_tokens":tokens,"output_tokens":2,"success":true});
        ledger.record(&event, stamp);
        ledger.record_call(&event, stamp - 1.0, stamp).unwrap();
    }
    let mut filter = UsageSeriesFilter {
        start: 0.0,
        end: 100.0,
        category: "all".into(),
        owner: None,
        provider: None,
        model: None,
        session: None,
        state: None,
    };
    let series = ledger.query_series(&filter, None, 1000.0, None).unwrap();
    assert_eq!(series["totals"]["input_tokens"], 60);
    assert_eq!(series["totals"]["requests"], 3);
    assert_eq!(
        series["totals"]["cost_nanos"],
        ledger.query(0.0, 100.0, "all", 1000.0, None).unwrap()["totals"]["cost_nanos"]
    );
    assert_eq!(
        series["series"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["input_tokens"].as_i64().unwrap())
            .sum::<i64>(),
        60
    );
    filter.provider = Some("first".into());
    filter.model = Some("route/model".into());
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0, None).unwrap()["totals"]["input_tokens"],
        30
    );
    filter.session = Some("b".into());
    filter.state = Some("completed".into());
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0, None).unwrap()["totals"]["input_tokens"],
        20
    );
    filter.state = Some("failed".into());
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0, None).unwrap()["totals"]["requests"],
        0
    );
    filter.state = None;
    filter.session = None;
    filter.category = "native".into();
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0, None).unwrap()["totals"]["requests"],
        0
    );
}

#[test]
fn historical_identity_requires_a_unique_matching_turn_and_route() {
    use emp_state::usage::ledger::UsageSeriesFilter;
    let root = tempfile::tempdir().unwrap();
    let ledger = UsageLedger::new(
        root.path().join("usage.sqlite3"),
        Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0)),
    );
    for (id, owner, origin, turn, tokens) in [
        ("r1", "account:a", "realtime", "one", 10),
        ("h1", "history:egg", "history", "one", 20),
        ("h2", "history:egg", "history", "unknown", 30),
        ("r2", "account:a", "realtime", "ambiguous", 40),
        ("r3", "account:b", "realtime", "ambiguous", 50),
        ("h3", "history:egg", "history", "ambiguous", 60),
    ] {
        ledger.record(&json!({"route":"responses","observation_id":id,"usage_owner":owner,"usage_category":"subscription","origin":origin,"usage_turn":turn,"route_model":"egg/model","upstream_model":"model","input_tokens":tokens,"output_tokens":1}),100.0);
    }
    let all = ledger.query(0.0, 200.0, "all", 1000.0, None).unwrap();
    assert_eq!(all["totals"]["requests"], 6);
    assert_eq!(all["totals"]["input_tokens"], 210);
    assert_eq!(
        ledger
            .query_owner(0.0, 200.0, "account:a", 1000.0, None)
            .unwrap()["totals"]["input_tokens"],
        70
    );
    let unresolved = all["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["owner"] == "history:egg")
        .unwrap();
    assert_eq!(unresolved["input_tokens"], 90);
    let filter = UsageSeriesFilter {
        start: 0.0,
        end: 200.0,
        category: "all".into(),
        owner: Some("account:a".into()),
        provider: None,
        model: None,
        session: None,
        state: None,
    };
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0, None).unwrap()["totals"]["input_tokens"],
        70
    );
    ledger
        .remember_account(
            "subscription",
            "account:a",
            "egg",
            "Egg",
            "e***@fixture.invalid",
        )
        .unwrap();
    let reopened = UsageLedger::new(
        root.path().join("usage.sqlite3"),
        Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0)),
    );
    assert_eq!(
        reopened.account_labels().unwrap()[0]["email"],
        "e***@fixture.invalid"
    );
    ledger
        .remember_account(
            "subscription",
            "account:b",
            "other",
            "Other",
            "e***@fixture.invalid",
        )
        .unwrap();
    assert_eq!(
        ledger.account_labels().unwrap().len(),
        2,
        "Same email must not merge different account IDs"
    );
}
