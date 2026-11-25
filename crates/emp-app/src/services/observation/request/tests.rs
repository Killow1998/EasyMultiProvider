use super::*;

fn records(root: &std::path::Path, diagnostics: &Diagnostics) -> Vec<Value> {
    diagnostics.journal.flush_blocking();
    std::fs::read_dir(root.join("logs"))
        .unwrap()
        .flat_map(|entry| {
            std::fs::read_to_string(entry.unwrap().path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<Value>>()
        })
        .collect()
}

#[test]
fn error_receipts_record_the_boundary_without_trusting_upstream_claims_or_logging_content() {
    let root = emp_test_support::fixture_dir().unwrap();
    let diagnostics = crate::tests::attached_diagnostics(root.path());
    let event = json!({"type":"error","error":{"code":"quota_exhausted","origin":"emp","message":"private-provider-detail"}});
    let mut receipt = RequestObservation::new(
        Arc::clone(&diagnostics),
        Some("0123456789abcdef".into()),
        None,
        "websocket",
        "responses",
    );
    receipt.phase(Phase::Execute);
    receipt
        .upstream_event_written(&event, Ok::<_, ()>(()))
        .unwrap();
    drop(receipt);
    let mut receipt = RequestObservation::new(
        Arc::clone(&diagnostics),
        Some("fedcba9876543210".into()),
        None,
        "websocket",
        "responses",
    );
    receipt.phase(Phase::ValidateInput);
    let event = json!({"type":"error","error":{"code":"invalid_request","origin":"emp","message":"private-input-detail"}});
    receipt
        .event_written(
            &event,
            Err::<(), _>(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        )
        .unwrap_err();
    drop(receipt);
    let journal = records(root.path(), &diagnostics);
    let failures: Vec<_> = journal
        .iter()
        .filter(|event| event["event"] == "response_error")
        .collect();
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["fields"]["error_origin"], "upstream");
    assert_eq!(failures[0]["fields"]["error_code"], "quota_exhausted");
    assert_eq!(failures[0]["fields"]["delivered"], true);
    assert_eq!(failures[1]["fields"]["error_origin"], "emp");
    assert_eq!(failures[1]["fields"]["phase"], "validate_input");
    assert_eq!(failures[1]["fields"]["delivered"], false);
    assert!(
        !serde_json::to_string(&journal)
            .unwrap()
            .contains("private-")
    );
}

#[test]
fn a_failed_terminal_write_is_not_completion_and_does_not_change_the_error() {
    let root = emp_test_support::fixture_dir().unwrap();
    let diagnostics = crate::tests::attached_diagnostics(root.path());
    let mut receipt = RequestObservation::new(
        Arc::clone(&diagnostics),
        Some("0123456789abcdef".into()),
        None,
        "sse",
        "responses",
    );
    receipt.phase(Phase::Execute);
    let terminal = json!({"type":"response.completed","response":{"output":"private-answer"}});
    let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "private-error");
    let result: std::io::Result<()> = receipt.event_written(&terminal, Err(error));
    assert_eq!(result.unwrap_err().to_string(), "private-error");
    drop(receipt);
    let records = records(root.path(), &diagnostics);
    let done = records
        .iter()
        .find(|r| r["event"] == "request_finished")
        .unwrap();
    assert_eq!(done["fields"]["delivery"], "write_failed");
    assert_eq!(done["fields"]["terminal_written"], false);
    assert_eq!(done["fields"]["downstream_terminal"], "unknown");
    assert!(done["fields"]["phase_ms"]["caller_access"].is_number());
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-")
    );
}

#[test]
fn partial_and_successful_delivery_remain_distinct_even_when_dropped() {
    for terminal in [false, true] {
        let root = emp_test_support::fixture_dir().unwrap();
        let diagnostics = crate::tests::attached_diagnostics(root.path());
        let mut receipt = RequestObservation::new(
            Arc::clone(&diagnostics),
            None,
            None,
            "websocket",
            "responses",
        );
        let event = json!({"type":if terminal { "response.incomplete" } else { "response.output_text.delta" },"delta":"private-output"});
        assert_eq!(receipt.event_written(&event, Ok::<_, ()>(7)), Ok(7));
        drop(receipt);
        let records = records(root.path(), &diagnostics);
        let done: Vec<_> = records
            .iter()
            .filter(|r| r["event"] == "request_finished")
            .collect();
        assert_eq!(done.len(), 1);
        assert_eq!(
            done[0]["fields"]["delivery"],
            if terminal {
                "terminal_written"
            } else {
                "partial"
            }
        );
        assert_eq!(
            done[0]["fields"]["downstream_terminal"],
            if terminal { "incomplete" } else { "unknown" }
        );
    }
}

#[test]
fn history_reason_survives_a_failed_write_and_unknown_text_is_not_logged() {
    for (reason, category, expected) in [
        (
            "compaction_summary_missing",
            "checkpoint",
            "compaction_summary_missing",
        ),
        (
            "private_history_content",
            "unknown",
            "history_reason_unknown",
        ),
    ] {
        let root = emp_test_support::fixture_dir().unwrap();
        let diagnostics = crate::tests::attached_diagnostics(root.path());
        let mut receipt = RequestObservation::new(
            Arc::clone(&diagnostics),
            Some("0123456789abcdef".into()),
            None,
            "sse",
            "responses",
        );
        receipt.phase(Phase::PrepareHistory);
        receipt.history_failed(&HistoryError::new(reason));
        let write: std::io::Result<()> = Err(std::io::ErrorKind::BrokenPipe.into());
        assert_eq!(
            receipt
                .event_written(&json!({"type":"response.failed"}), write)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::BrokenPipe
        );
        drop(receipt);
        let records = records(root.path(), &diagnostics);
        let failure = records
            .iter()
            .find(|r| r["event"] == "history_reconstruction_failed")
            .unwrap();
        let done = records
            .iter()
            .find(|r| r["event"] == "request_finished")
            .unwrap();
        assert_eq!(
            failure["fields"]["request_id"],
            done["fields"]["request_id"]
        );
        assert_eq!(failure["fields"]["reason"], expected);
        assert_eq!(
            done["fields"]["history_failure"],
            json!({"phase":"prepare_history", "category":category, "reason":expected})
        );
        assert_eq!(done["fields"]["delivery"], "write_failed");
        assert!(!serde_json::to_string(&records).unwrap().contains("private"));
    }
}
