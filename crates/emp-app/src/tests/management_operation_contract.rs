//! The real management endpoints retain their contracts and expose partial work.
use super::internal_events_contract::journal;
use super::*;

fn completed(root: &Path, operation: &str) -> Value {
    let records = journal(root);
    let done = records
        .iter()
        .rev()
        .find(|record| {
            record["event"] == "operation_finished" && record["fields"]["operation"] == operation
        })
        .unwrap_or_else(|| panic!("missing {operation} receipt"));
    let id = &done["fields"]["operation_id"];
    assert!(id.as_str().is_some_and(|id| id.len() == 16));
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "operation_started"
                && &record["fields"]["operation_id"] == id)
            .count(),
        1
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record["event"] == "operation_finished"
                && &record["fields"]["operation_id"] == id)
            .count(),
        1
    );
    done["fields"].clone()
}

#[test]
fn saved_preference_and_failed_catalog_are_distinct_and_linked_to_http() {
    let (root, server) = test_server();
    let path = crate::services::catalog::generated_catalog_path(&server.state);
    assert!(path.starts_with(canonical_root(&root)));
    assert!(
        server
            .state
            .backend
            .accounts
            .native_auth_path
            .starts_with(canonical_root(&root))
    );
    if path.is_file() {
        std::fs::remove_file(&path).unwrap();
    }
    std::fs::create_dir_all(&path).unwrap();
    let reply = post(
        &server,
        "/api/catalog/context-preference",
        br#"{"catalog_show_context":false}"#,
        &[&session_header(&server), "X-EMP-Request-ID: private-spoof"],
    );
    assert!(reply.starts_with("HTTP/1.1 500"), "{reply}");
    let saved = load_configuration(Some(&server.state.backend.configuration.config_path)).unwrap();
    assert_eq!(saved["catalog_show_context"], false);
    assert_eq!(
        server.state.backend.configuration.snapshot().unwrap()["catalog_show_context"],
        false
    );
    server.shutdown().unwrap();
    let done = completed(root.path(), "catalog_preference");
    assert_eq!(done["outcome"], "failed");
    assert_eq!(done["checks"]["configuration_committed"], true);
    assert_eq!(done["checks"]["preference_matches_saved"], true);
    assert_eq!(done["last_stage"], "publish_catalog");
    assert_eq!(
        done["stages"].as_array().unwrap().last().unwrap()["outcome"],
        "failed"
    );
    assert_eq!(done["client_effect"], "unknown");
    let records = journal(root.path());
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "http_request_completed"
                && record["fields"]["request_id"] == done["request_id"]
                && record["fields"]["status"] == 500)
    );
    assert!(
        !serde_json::to_string(&records)
            .unwrap()
            .contains("private-spoof")
    );
}

#[test]
fn rejected_commands_do_not_claim_side_effects_or_expose_input() {
    let (root, server) = test_server();
    let before = server.state.backend.configuration.snapshot().unwrap();
    for (path, body, status) in [
        (
            "/api/accounts/import",
            br#"{"auth_json":"private-credential","id":"private-account"}"#.as_slice(),
            400,
        ),
        (
            "/api/providers/discover",
            br#"{"provider":"private-provider"}"#.as_slice(),
            400,
        ),
        ("/api/accounts/private-account/quota", b"{}".as_slice(), 503),
        ("/api/integration/enable", b"{}".as_slice(), 409),
    ] {
        let reply = post(&server, path, body, &[&session_header(&server)]);
        assert!(reply.starts_with(&format!("HTTP/1.1 {status}")), "{reply}");
    }
    assert_eq!(
        server.state.backend.configuration.snapshot().unwrap(),
        before
    );
    server.shutdown().unwrap();
    for operation in [
        "account_import",
        "model_discovery",
        "quota_read",
        "integration_enable",
    ] {
        let done = completed(root.path(), operation);
        assert_eq!(done["outcome"], "failed");
        assert_eq!(done["client_effect"], "unknown");
        let stages = done["stages"].as_array().unwrap();
        if operation == "integration_enable" {
            assert_eq!(stages.len(), 1);
            assert_eq!(stages[0]["stage"], "read_summary");
        } else {
            assert!(stages.is_empty());
        }
    }
    let text = serde_json::to_string(&journal(root.path())).unwrap();
    for private in ["private-credential", "private-account", "private-provider"] {
        assert!(!text.contains(private), "journal disclosed {private}");
    }
}

#[test]
fn configuration_application_is_not_runtime_or_routing_verification() {
    let (root, server) = configured_server("http://127.0.0.1:9/v1");
    let reply = post(
        &server,
        "/api/integration/enable",
        br#"{"confirm_reload":true}"#,
        &[&session_header(&server)],
    );
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let status: Value = serde_json::from_str(reply.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(status["configuration"]["state"], "emp_applied");
    assert_ne!(status["runtime"]["state"], "catalog_loaded");
    let done = completed(root.path(), "integration_enable");
    assert_eq!(done["outcome"], "completed");
    assert_eq!(done["checks"]["saved_configuration_matches_target"], true);
    assert!(done["checks"]["runtime_catalog_matches_target"].is_null());
    assert!(done["checks"]["request_routing_verified"].is_null());
    assert!(done["checks"]["desktop_effect_verified"].is_null());
    server.shutdown().unwrap();
}
