use emp_state::usage::{
    ledger::{CallFilter, UsageLedger, UsageSeriesFilter},
    pricing::PriceCatalog,
};
use serde_json::json;
use std::sync::Arc;

#[test]
fn nested_periods_match_fresh_reports_and_database_writes_invalidate_both_projections() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("usage.db");
    let prices = Arc::new(PriceCatalog::new(root.path().join("prices.json"), 0.0));
    prices.replace(
        json!({"model":{"input_cost_per_token":"0.001","output_cost_per_token":"0.002"}}),
        1.0,
    );
    let ledger = UsageLedger::new(path.clone(), prices.clone());
    let end = 4_000_000.0;
    for (index, seconds) in [30. * 86400., 7. * 86400., 86400., 3600., 3599., 1., 0.]
        .into_iter()
        .enumerate()
    {
        let event = json!({"request_id":format!("row-{index}"),"route":"responses","model_id":"model",
            "upstream_model":"model","provider_id":if index%2==0 {"a"} else {"b"},
            "usage_category":"external","usage_owner":if index%2==0 {"a"} else {"b"},
            "thread_id":"thread", "success":true,"input_tokens":index*10,"output_tokens":20,
            "cached_input_tokens":0,"first_content_ms":index*100,"generation_ms":1000,
            "generation_tokens_per_second":index*5});
        ledger.record(&event, end - seconds);
        ledger
            .record_call(&event, end - seconds, end - seconds + 1.)
            .unwrap();
    }
    let mut calls = CallFilter {
        start: 0.,
        end,
        category: None,
        provider: None,
        account: None,
        model: None,
        models: vec![],
        session: None,
        state: None,
        request: None,
        offset: 0,
        limit: 50,
        models_offset: 0,
        models_sort: "calls".into(),
    };
    let mut usage = UsageSeriesFilter {
        start: 0.,
        end,
        category: "all".into(),
        owner: None,
        provider: None,
        model: None,
        session: None,
        state: None,
    };
    let reference_path = root.path().join("reference.db");
    rusqlite::Connection::open(&path)
        .unwrap()
        .backup(rusqlite::MAIN_DB, &reference_path, None)
        .unwrap();
    for duration in [30. * 86400., 7. * 86400., 86400., 3600.] {
        calls.start = end - duration;
        usage.start = calls.start;
        let fresh = UsageLedger::new(reference_path.clone(), prices.clone());
        let report = ledger.query_calls(&calls).unwrap();
        assert_eq!(report, fresh.query_calls(&calls).unwrap());
        assert_eq!(
            ledger.query_series(&usage, None, end, None).unwrap(),
            fresh.query_series(&usage, None, end, None).unwrap()
        );
        if duration == 3600. {
            assert_eq!(report["total"], 3);
            assert_eq!(report["summary"]["ttft_ms"], 400.);
            assert_eq!(report["summary"]["tokens_per_second"], 20.);
            assert_eq!(
                ledger.query_series(&usage, None, end, None).unwrap()["totals"]["input_tokens"],
                120
            );
        }
    }
    calls.provider = Some("a".into());
    usage.provider = Some("a".into());
    let fresh = UsageLedger::new(path.clone(), prices.clone());
    assert_eq!(
        ledger.query_calls(&calls).unwrap(),
        fresh.query_calls(&calls).unwrap()
    );
    assert_eq!(
        ledger.query_series(&usage, None, end, None).unwrap(),
        fresh.query_series(&usage, None, end, None).unwrap()
    );
    // Writes from another connection include retroactive correction, not just newer timestamps.
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute("UPDATE call_records SET facts=json_set(facts,'$.first_content_ms',999) WHERE request_id='row-4'",[]).unwrap();
    connection
        .execute(
            "UPDATE usage_events SET input_tokens=999 WHERE owner='a'",
            [],
        )
        .unwrap();
    let fresh = UsageLedger::new(path.clone(), prices.clone());
    assert_eq!(
        ledger.query_calls(&calls).unwrap()["summary"]["ttft_ms"],
        999.
    );
    assert_eq!(
        ledger.query_calls(&calls).unwrap(),
        fresh.query_calls(&calls).unwrap()
    );
    assert_eq!(
        ledger.query_series(&usage, None, end, None).unwrap(),
        fresh.query_series(&usage, None, end, None).unwrap()
    );
    assert_eq!(
        ledger.query_series(&usage, None, end, None).unwrap()["totals"]["input_tokens"],
        999
    );
    // A request arriving inside a previously loaded window must also become visible.
    ledger.record_call(&json!({"request_id":"new","route":"responses","provider_id":"a","model_id":"model","success":true}),end-2.,end-1.).unwrap();
    assert_eq!(ledger.query_calls(&calls).unwrap()["total"], 2);
}
