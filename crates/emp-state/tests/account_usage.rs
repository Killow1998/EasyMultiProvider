use emp_state::usage::{ledger::UsageLedger, pricing::PriceCatalog};
use serde_json::json;
use std::sync::Arc;

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
        .query_owner(0.0, 1000.0, "account:selected", 1000.0)
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
    assert_eq!(
        ledger.query(0.0, 1000.0, "all", 1000.0).unwrap()["totals"]["requests"],
        3
    );
    assert!(ledger.query_owner(0.0, 1000.0, "", 1000.0).is_err());
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
    let series = ledger.query_series(&filter, None, 1000.0).unwrap();
    assert_eq!(series["totals"]["input_tokens"], 60);
    assert_eq!(series["totals"]["requests"], 3);
    assert_eq!(
        series["totals"]["cost_nanos"],
        ledger.query(0.0, 100.0, "all", 1000.0).unwrap()["totals"]["cost_nanos"]
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
        ledger.query_series(&filter, None, 1000.0).unwrap()["totals"]["input_tokens"],
        30
    );
    filter.session = Some("b".into());
    filter.state = Some("completed".into());
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0).unwrap()["totals"]["input_tokens"],
        20
    );
    filter.state = Some("failed".into());
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0).unwrap()["totals"]["requests"],
        0
    );
    filter.state = None;
    filter.session = None;
    filter.category = "native".into();
    assert_eq!(
        ledger.query_series(&filter, None, 1000.0).unwrap()["totals"]["requests"],
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
    let all = ledger.query(0.0, 200.0, "all", 1000.0).unwrap();
    assert_eq!(all["totals"]["requests"], 6);
    assert_eq!(all["totals"]["input_tokens"], 210);
    assert_eq!(
        ledger.query_owner(0.0, 200.0, "account:a", 1000.0).unwrap()["totals"]["input_tokens"],
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
        ledger.query_series(&filter, None, 1000.0).unwrap()["totals"]["input_tokens"],
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
