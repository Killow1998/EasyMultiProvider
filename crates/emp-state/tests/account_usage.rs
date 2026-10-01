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
