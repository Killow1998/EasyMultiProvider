//! Model checks must not displace the conversation samples used by diagnostics.
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn model_checks_do_not_evict_conversation_samples_after_restart() {
    let root = emp_test_support::fixture_dir().unwrap();
    let parts = || {
        std::fs::read_dir(root.path().join("logs"))
            .unwrap()
            .map(|part| part.unwrap().path())
            .collect::<BTreeSet<_>>()
    };
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
    let first_parts = parts();
    let first_run_id = first_parts
        .iter()
        .find_map(|part| shutdown_run_id(part))
        .expect("first run wrote shutdown_complete");

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
    let second_run_id = parts()
        .difference(&first_parts)
        .find_map(|part| shutdown_run_id(part))
        .expect("restart wrote shutdown_complete to a new journal part");
    assert_ne!(second_run_id, first_run_id);
}

fn shutdown_run_id(part: &Path) -> Option<String> {
    std::fs::read_to_string(part)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|record| record["event"] == "shutdown_complete")
        .map(|record| record["run_id"].as_str().unwrap().to_owned())
}
