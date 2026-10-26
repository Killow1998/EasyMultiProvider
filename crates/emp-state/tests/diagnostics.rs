//! Model checks must not displace the conversation samples used by diagnostics.
use emp_state::diagnostics::Diagnostics;
use serde_json::json;

#[test]
fn model_checks_do_not_evict_conversation_samples_after_restart() {
    let root = tempfile::tempdir().unwrap();
    let diagnostics = Diagnostics::new(root.path());
    diagnostics.record(&json!({"route":"responses", "request_id":"0123456789abcdef"}));
    for index in 0..600 {
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
    let restarted = Diagnostics::new(root.path()).snapshot();
    assert_eq!(restarted["sample_count"], 1);
    assert_eq!(restarted["records"], snapshot["records"]);
    assert_eq!(restarted["probes"], snapshot["probes"]);
}
