use super::*;
use crate::server::test_hooks::ReportGate;
use std::sync::atomic::AtomicBool;
use tokio::sync::oneshot;

pub(super) fn report_gate(
    fixture: &Fixture,
) -> (oneshot::Receiver<Arc<AtomicBool>>, oneshot::Sender<()>) {
    let (query_paused, received) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    *fixture
        .server
        .state
        .lifecycle
        .test_hooks
        .report_gate
        .lock()
        .unwrap() = Some(ReportGate {
        query_paused,
        release: wait,
    });
    (received, release)
}

pub(super) fn seed_reports(fixture: &Fixture) {
    let ledger = &fixture.server.state.backend.usage.ledger;
    ledger.query(1.0, 10.0, "all", system_now(), None).unwrap();
    let database =
        rusqlite::Connection::open(fixture.directory.path().join("state/usage.sqlite3")).unwrap();
    database.execute_batch(
        "INSERT INTO call_records(request_id,operation,started_at,finished_at,provider_id,account_id,model_id,session_id,state,facts)
         VALUES('call-fixture','responses',2,3,'fixture','','model','','completed','{\"request_id\":\"call-fixture\"}');
         INSERT INTO usage_events(id,observed_at,category,owner,model,service_tier,operation,rates,origin,usage_turn,route_model,match_key)
         VALUES('usage-fixture',2,'external','fixture','model','','responses','{}','realtime','','model','');",
    ).unwrap();
}
