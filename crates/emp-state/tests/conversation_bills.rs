use emp_state::usage::{
    ledger::{SessionBillPage, SessionScope, UsageLedger, UsageSeriesFilter},
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

fn event(id: &str, session: &str, parent: &str, kind: &str, input: i64) -> Value {
    json!({"observation_id":id,"request_id":id,"thread_id":session,"session_id":session,
        "parent_thread_id":parent,"session_kind":kind,"success":true,
        "model_id":if kind == "review" { "codex-auto-review" } else { "example/model" },
        "route":"responses","route_model":"example/model","upstream_model":"priced",
        "usage_category":"external","usage_owner":"fixture",
        "input_tokens":input,"output_tokens":3,"cached_input_tokens":0})
}

fn main_conversation(scope: &mut SessionScope, id: &str) {
    scope.observe(id, "", "conversation");
    scope.0.get_mut(id).unwrap().name = format!("Conversation {id}");
}

#[test]
fn conversation_bills_include_related_work_without_changing_frozen_totals() {
    let root = tempfile::tempdir().unwrap();
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0));
    prices.replace(
        json!({"priced":{"input_cost_per_token":"0.000001",
        "output_cost_per_token":"0.000002","cache_read_input_token_cost":"0"}}),
        10.0,
    );
    let path = root.path().join("usage.sqlite3");
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    for (id, parent, kind, input) in [
        ("main", "", "", 100),
        ("review", "main", "review", 50),
        ("sidechat", "main", "sidechat", 20),
        ("global-memory", "main", "memory_consolidation", 25),
        ("child", "bridge", "subagent", 30),
        ("other", "", "", 40),
        ("orphan", "", "", 60),
        ("orphan-review", "", "review", 70),
        ("conflict", "main", "subagent", 80),
        ("cycle-one", "cycle-two", "subagent", 90),
    ] {
        let mut record = event(id, id, parent, kind, input);
        if id == "review" {
            record.as_object_mut().unwrap().remove("session_kind");
        }
        if id == "orphan" {
            record["upstream_model"] = json!("unpriced");
        }
        ledger.record(&record, 100.0);
        ledger.record_call(&record, 99.0, 100.0).unwrap();
    }
    // The parent has no usage in this period, but its existing receipt links the grandchild.
    ledger
        .record_call(
            &event("old-bridge", "bridge", "main", "subagent", 0),
            -20.0,
            -19.0,
        )
        .unwrap();
    ledger
        .record_call(
            &event("cycle-edge", "cycle-two", "cycle-one", "subagent", 0),
            99.0,
            100.0,
        )
        .unwrap();
    ledger
        .record_call(
            &event("conflicting-edge", "conflict", "other", "subagent", 0),
            99.0,
            100.0,
        )
        .unwrap();
    let original = ledger.query(0.0, 200.0, "all", 1000.0).unwrap()["totals"].clone();
    drop(ledger);
    prices.replace(
        json!({"priced":{"input_cost_per_token":"0.01",
        "output_cost_per_token":"0.02"}}),
        200.0,
    );
    let ledger = UsageLedger::new(path, prices);
    let mut scope = ledger.session_scope(&filter()).unwrap();
    main_conversation(&mut scope, "main");
    main_conversation(&mut scope, "other");
    assert_eq!(scope.root("child"), Some("main"));
    assert_eq!(scope.root("global-memory"), None);
    assert_eq!(scope.0["review"].kind, "review");
    let bills = ledger
        .query_sessions(&filter(), &page(), Some(&scope), 1000.0)
        .unwrap();
    assert_eq!(bills["total"], 2);
    assert_eq!(bills["unassigned"]["requests"], 5);
    for (field, value) in original.as_object().unwrap() {
        assert_eq!(&bills["totals"][field], value, "{field}");
    }
    assert_eq!(
        ledger.query(0.0, 200.0, "all", 1000.0).unwrap()["totals"],
        original
    );
    let mut selected = filter();
    selected.session = Some("main".into());
    let detail = ledger
        .query_series(&selected, Some(&scope), 1000.0)
        .unwrap();
    assert_eq!(detail["totals"]["requests"], 4);
    assert_eq!(detail["totals"]["input_tokens"], 200);
    assert_eq!(detail["components"][0]["thread_id"], "main");
    for (id, kind) in [
        ("main", "conversation"),
        ("review", "review"),
        ("sidechat", "sidechat"),
        ("child", "subagent"),
    ] {
        assert!(
            detail["components"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["thread_id"] == id && row["session_kind"] == kind)
        );
    }
    for field in [
        "requests",
        "input_tokens",
        "output_tokens",
        "cost_nanos",
        "priced_requests",
    ] {
        let sum: i64 = detail["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[field].as_i64().unwrap())
            .sum();
        assert_eq!(Some(sum), detail["totals"][field].as_i64(), "{field}");
        let billed: i64 = bills["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[field].as_i64().unwrap())
            .sum();
        assert_eq!(
            billed + bills["unassigned"][field].as_i64().unwrap(),
            original[field].as_i64().unwrap()
        );
    }
    selected.provider = Some("another-service".into());
    assert_eq!(
        ledger
            .query_series(&selected, Some(&scope), 1000.0)
            .unwrap()["totals"]["requests"],
        0
    );
}

#[test]
fn inherited_events_are_billed_once_only_when_all_copies_agree_on_a_conversation() {
    let root = tempfile::tempdir().unwrap();
    let ledger = UsageLedger::new(
        root.path().join("usage.sqlite3"),
        Arc::new(PriceCatalog::new(root.path().join("prices.json"), 1000.0)),
    );
    let mut shared = event("shared", "main", "", "", 100);
    ledger.record(&shared, 100.0);
    shared["session_id"] = json!("child");
    shared["thread_id"] = json!("child");
    shared["parent_thread_id"] = json!("main");
    shared["session_kind"] = json!("sidechat");
    ledger.record(&shared, 100.0);
    ledger.record_call(&shared, 99.0, 100.0).unwrap();
    let mut scope = ledger.session_scope(&filter()).unwrap();
    main_conversation(&mut scope, "main");
    assert!(scope.0.contains_key("main") && scope.0.contains_key("child"));
    let bills = ledger
        .query_sessions(&filter(), &page(), Some(&scope), 1000.0)
        .unwrap();
    assert_eq!(bills["total"], 1);
    assert_eq!(bills["records"][0]["requests"], 1);
    assert!(bills["unassigned"].is_null());
    let mut selected = filter();
    selected.session = Some("main".into());
    let detail = ledger
        .query_series(&selected, Some(&scope), 1000.0)
        .unwrap();
    assert_eq!(detail["components"].as_array().unwrap().len(), 1);
    assert_eq!(detail["components"][0]["session_kind"], "shared");
    shared["session_id"] = json!("another-main");
    shared["thread_id"] = json!("another-main");
    ledger.record(&shared, 100.0);
    main_conversation(&mut scope, "another-main");
    let conflicted = ledger
        .query_sessions(&filter(), &page(), Some(&scope), 1000.0)
        .unwrap();
    assert_eq!(conflicted["total"], 0);
    assert_eq!(conflicted["unassigned"]["requests"], 1);
    assert_eq!(conflicted["totals"], bills["totals"]);
}
