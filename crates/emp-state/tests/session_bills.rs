use emp_state::usage::{
    ledger::{SessionBillPage, UsageLedger, UsageSeriesFilter},
    pricing::PriceCatalog,
};
use serde_json::{Value, json};
use std::sync::Arc;

fn filter() -> UsageSeriesFilter {
    UsageSeriesFilter {
        start: 0.0,
        end: 200.0,
        category: "all".into(),
        owner: None,
        provider: None,
        model: None,
        session: None,
        state: None,
    }
}
fn page() -> SessionBillPage {
    SessionBillPage {
        offset: 0,
        limit: 20,
        sort: "recent".into(),
        matching_sessions: None,
    }
}
fn event(id: &str, session: &str, model: &str, input: i64) -> Value {
    json!({"observation_id":id,"request_id":id,"session_id":session,
        "route":"responses","route_model":"route/model","upstream_model":model,
        "usage_category":"external","usage_owner":"fixture","input_tokens":input,
        "output_tokens":20,"cached_input_tokens":40,"cache_write_tokens":10,"reasoning_tokens":4})
}

#[test]
fn session_bills_partition_reconciled_usage_without_filling_unknowns_or_repricing() {
    let root = tempfile::tempdir().unwrap();
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    prices.replace(
        json!({"priced":{"input_cost_per_token":"0.000001",
        "output_cost_per_token":"0.000002","cache_read_input_token_cost":"0.0000001",
        "cache_creation_input_token_cost":"0.000002"}}),
        100.0,
    );
    let path = root.path().join("usage.sqlite3");
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    let mut live = event("live", "one", "priced", 100);
    live["usage_turn"] = json!("shared-turn");
    ledger.record(&live, 100.0);
    let mut history = live.clone();
    history["observation_id"] = json!("history");
    history["origin"] = json!("history");
    ledger.record(&history, 100.0);
    ledger.record(&event("second", "two", "unpriced", 2000), 110.0);
    let copied = event("copy", "one", "priced", 50);
    ledger.record(&copied, 120.0);
    let mut duplicate = copied.clone();
    duplicate["session_id"] = json!("two");
    ledger.record(&duplicate, 120.0);
    ledger.record(&event("unassigned", "", "priced", 100), 130.0);
    let mut incomplete = event("incomplete", "one", "priced", 11);
    incomplete.as_object_mut().unwrap().remove("output_tokens");
    ledger.record(&incomplete, 140.0);
    ledger.record(&event("outside", "three", "priced", 999), 200.0);
    let before = ledger.query(0.0, 200.0, "all", 1000.0).unwrap();
    let mut selected = filter();
    let mut paging = page();
    let bills = ledger.query_sessions(&selected, &paging, 1000.0).unwrap();
    assert_eq!(bills["total"], 2);
    assert_eq!(bills["totals"]["requests"], 5);
    assert_eq!(bills["unassigned"]["requests"], 2);
    for field in [
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cost_nanos",
        "requests",
        "priced_requests",
    ] {
        assert_eq!(bills["totals"][field], before["totals"][field], "{field}");
    }
    selected.session = Some("one".into());
    let detail = ledger.query_series(&selected, 1000.0).unwrap();
    assert_eq!(detail["totals"]["requests"], 2);
    assert_eq!(detail["totals"]["input_tokens"], 111);
    assert_eq!(detail["totals"]["reported_requests"], 1);
    assert_eq!(detail["totals"]["priced_requests"], 1);
    assert_eq!(detail["totals"]["cost_nanos"], 114000);
    selected.session = None;
    paging.sort = "tokens".into();
    paging.limit = 1;
    let first = ledger.query_sessions(&selected, &paging, 1000.0).unwrap();
    assert_eq!(first["records"][0]["session_id"], "two");
    assert_eq!(first["records"][0]["unpriced_requests"], 1);
    paging.offset = 1;
    assert_eq!(
        ledger.query_sessions(&selected, &paging, 1000.0).unwrap()["records"][0]["session_id"],
        "one"
    );
    paging.matching_sessions = Some(vec!["one".into()]);
    paging.offset = 0;
    assert_eq!(
        ledger.query_sessions(&selected, &paging, 1000.0).unwrap()["total"],
        1
    );
    selected.provider = Some("other".into());
    assert_eq!(
        ledger.query_sessions(&selected, &paging, 1000.0).unwrap()["totals"]["requests"],
        0
    );
    drop(ledger);
    let reopened = UsageLedger::new(path, prices);
    assert_eq!(
        reopened.query_sessions(&filter(), &page(), 1000.0).unwrap(),
        bills
    );
}

#[test]
fn existing_call_receipts_backfill_session_links_on_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.sqlite3");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    let mut record = event("old-call", "", "unpriced", 100);
    ledger.record(&record, 100.0);
    record["thread_id"] = json!("old-session");
    record["success"] = json!(true);
    ledger.record_call(&record, 99.0, 100.0).unwrap();
    drop(ledger);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE usage_sessions")
        .unwrap();
    let upgraded = UsageLedger::new(path, prices);
    let bills = upgraded.query_sessions(&filter(), &page(), 1000.0).unwrap();
    assert_eq!(bills["total"], 1);
    assert_eq!(bills["records"][0]["session_id"], "old-session");
    assert_eq!(bills["totals"]["requests"], 1);
    assert!(bills["unassigned"].is_null());
}
