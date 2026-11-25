//! Model checks must not displace the conversation samples used by diagnostics.
use serde_json::json;

#[test]
fn model_checks_do_not_evict_conversation_samples_after_restart() {
    let root = emp_test_support::fixture_dir().unwrap();
    let diagnostics = super::attached_diagnostics(root.path());
    diagnostics.record(&json!({"route":"responses", "request_id":"0123456789abcdef"}));
    for index in 0..600 {
        if index == 300 {
            diagnostics.journal.flush_blocking();
        }
        let probe = json!({"route":"model_test", "operation":"model_test",
            "request_id":format!("{index:016x}"), "outcome":"completed"});
        diagnostics.record(&probe);
        diagnostics
            .journal
            .event("info", "request_finished", &probe);
    }
    let snapshot = diagnostics.snapshot();
    assert_eq!(snapshot["sample_count"], 1);
    assert_eq!(snapshot["records"][0]["request_id"], "0123456789abcdef");
    let probes = snapshot["probes"].as_array().unwrap();
    assert_eq!(probes.len(), 64);
    assert_eq!(
        probes.first().unwrap()["request_id"],
        format!("{:016x}", 536)
    );
    assert_eq!(
        probes.last().unwrap()["request_id"],
        format!("{:016x}", 599)
    );
    diagnostics.journal.close_with_event_blocking(
        "info",
        "shutdown_complete",
        &json!({"success":true}),
    );
    drop(diagnostics);

    let restarted = super::attached_diagnostics(root.path());
    let after_restart = restarted.snapshot();
    assert_eq!(after_restart["sample_count"], 1);
    assert_eq!(after_restart["records"], snapshot["records"]);
    assert_eq!(after_restart["probes"], snapshot["probes"]);
    restarted.journal.close_with_event_blocking(
        "info",
        "shutdown_complete",
        &json!({"success":true}),
    );
}
