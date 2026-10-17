//! Observable continuation, isolation, cancellation and upload contracts.
use super::*;
use sha2::{Digest, Sha256};
mod collaboration;
mod fixture;
use fixture::*;

#[test]
fn native_large_context_keeps_twenty_incremental_turns_for_imported_and_default_accounts() {
    for imported in [true, false] {
        let upstream = Upstream::start(10 * 1024 * 1024);
        let (directory, server, model) = server(&upstream, imported);
        let mut client = connect(&server, "large-context-fixture");
        let mut input = initial_input();
        let initial_bytes = serde_json::to_vec(&input).unwrap().len();
        let mut hash = Sha256::new();
        append_input(&mut hash, &json!({"input":input}));
        let mut expected = vec![format!("{:x}", hash.clone().finalize())];
        let first = turn(
            &mut client,
            &json!({"type":"response.create","model":model,"input":input}),
        );
        assert_eq!(
            first["type"], "response.completed",
            "imported={imported}: {first}"
        );
        let mut prior = first["response"]["id"].as_str().unwrap().to_owned();
        let mut recoveries = 0;
        for step in 0..20 {
            let delta = json!({"role":"user","content":format!("small addition {step}")});
            input.push(delta.clone());
            append_input(&mut hash, &json!({"input":[delta]}));
            expected.push(format!("{:x}", hash.clone().finalize()));
            let mut result = turn(
                &mut client,
                &json!({"type":"response.create","model":model,"previous_response_id":prior,"input":[delta]}),
            );
            if result["error"]["code"] == "previous_response_not_found" {
                // Emulate Codex's observable full-history recovery, including reconnect.
                recoveries += 1;
                client.close();
                client = connect(&server, "large-context-fixture");
                result = turn(
                    &mut client,
                    &json!({"type":"response.create","model":model,"input":input}),
                );
            }
            assert_eq!(result["type"], "response.completed");
            prior = result["response"]["id"].as_str().unwrap().to_owned();
        }
        client.close();
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(
            stats.generated_hashes, expected,
            "every text, image and tool item must survive in order"
        );
        eprintln!(
            "continuation imported={imported} initial_bytes={initial_bytes} upstream_application_bytes={} recoveries={recoveries} ws_connections={} http_requests={} warmups={}",
            stats.application_bytes, stats.ws_connections, stats.http_requests, stats.warmups
        );
        assert_eq!(
            recoveries, 0,
            "valid continuations must not trigger full replay"
        );
        assert_eq!(stats.http_requests, 0);
        assert_eq!(stats.ws_connections, 1);
        assert!(
            stats.application_bytes < initial_bytes + 128 * 1024,
            "upload must scale with new input"
        );
        let journal = crate::tests::internal_events_contract::journal(directory.path());
        let receipts = receipts(&journal);
        assert_eq!(
            receipts.len(),
            21,
            "exactly one byte receipt per downstream request"
        );
        assert_eq!(
            receipts
                .iter()
                .map(|r| r["upstream_decoded_bytes_sent"].as_u64().unwrap())
                .sum::<u64>(),
            stats.application_bytes as u64
        );
        assert_eq!(
            receipts
                .iter()
                .filter(|r| r["connection_reused"] == true)
                .count(),
            20
        );
        assert_eq!(
            receipts
                .iter()
                .map(|r| r["connections_opened"].as_u64().unwrap())
                .sum::<u64>(),
            1
        );
        for receipt in receipts {
            assert_eq!(receipt["byte_scope"], "websocket_text_payload");
            assert_eq!(receipt["permessage_deflate"], false);
            assert_eq!(
                receipt["upstream_decoded_bytes_sent"],
                receipt["upstream_encoded_bytes_sent"]
            );
            assert!(receipt["reason"].is_null());
        }
        assert_private(&journal);
    }
}

fn receipts(journal: &[Value]) -> Vec<&Value> {
    journal
        .iter()
        .filter(|event| event["event"] == "request_finished")
        .filter_map(|event| event["fields"].get("native_continuation"))
        .collect()
}

fn assert_private(journal: &[Value]) {
    let text = serde_json::to_string(journal).unwrap();
    for private in [
        "fixture-access",
        "fixture-owner",
        "fixture-call",
        "synthetic-0",
        "data:image",
        "resp_1_",
        "11111111-1111-4111-8111-111111111111",
    ] {
        assert!(
            !text.contains(private),
            "private fixture data appeared in diagnostics"
        );
    }
}

fn small_request(model: &str, prior: Option<&str>, window: &str) -> Value {
    json!({"type":"response.create","model":model,"previous_response_id":prior,
        "input":[{"role":"user","content":"small synthetic input"}],
        "client_metadata":{"x-codex-turn-metadata":serde_json::to_string(&json!({"turn_id":"33333333-3333-4333-8333-333333333333","window_id":window})).unwrap()}})
}

fn completed(event: Value) -> String {
    assert_eq!(event["type"], "response.completed", "{event}");
    event["response"]["id"].as_str().unwrap().into()
}

#[test]
fn rejected_scope_route_identity_and_foreign_prior_preserve_isolated_valid_sessions() {
    let upstream = Upstream::start(10 * 1024 * 1024);
    let (directory, server, model) = server(&upstream, false);
    let mut a = connect(&server, "11111111-1111-4111-8111-111111111111");
    let mut b = connect(&server, "44444444-4444-4444-8444-444444444444");
    let prior_a = completed(turn(&mut a, &small_request(&model, None, "window-one")));
    let prior_b = completed(turn(&mut b, &small_request(&model, None, "window-one")));
    for (prior, window, reason) in [
        (&prior_b, "window-one", "prior_response_mismatch"),
        (&prior_a, "window-two", "scope_mismatch"),
    ] {
        let error = turn(&mut a, &small_request(&model, Some(prior), window));
        assert_eq!(error["error"]["code"], "previous_response_not_found");
        assert_eq!(error["error"]["failure_reason"], reason);
    }
    let auth = directory.path().join("codex/auth.json");
    let original_auth = std::fs::read(&auth).unwrap();
    std::fs::write(
        &auth,
        br#"{"tokens":{"access_token":"another-fixture","account_id":"different-owner"}}"#,
    )
    .unwrap();
    let error = turn(&mut a, &small_request(&model, Some(&prior_a), "window-one"));
    assert_eq!(error["error"]["failure_reason"], "identity_mismatch");
    std::fs::write(&auth, original_auth).unwrap();
    let original_base = {
        let mut config = server
            .state
            .backend
            .configuration
            .test_config()
            .lock()
            .unwrap();
        let original = config["codex_base_url"].clone();
        config["codex_base_url"] = json!(format!("http://{}/other", upstream.address));
        original
    };
    let error = turn(&mut a, &small_request(&model, Some(&prior_a), "window-one"));
    assert_eq!(error["error"]["failure_reason"], "route_mismatch");
    server
        .state
        .backend
        .configuration
        .test_config()
        .lock()
        .unwrap()["codex_base_url"] = original_base;
    completed(turn(
        &mut a,
        &small_request(&model, Some(&prior_a), "window-one"),
    ));
    completed(turn(
        &mut b,
        &small_request(&model, Some(&prior_b), "window-one"),
    ));
    // Explicit null establishes a fresh full-history base across a real scope change.
    let reset = completed(turn(&mut a, &small_request(&model, None, "window-two")));
    completed(turn(
        &mut a,
        &small_request(&model, Some(&reset), "window-two"),
    ));
    a.close();
    b.close();
    server.shutdown().unwrap();
    let stats = upstream.traffic.lock().unwrap();
    assert_eq!(stats.ws_connections, 2);
    assert_eq!(stats.http_requests, 0);
    assert_eq!(
        stats.generated_hashes.len(),
        6,
        "rejected requests must never reach an upstream"
    );
    let journal = crate::tests::internal_events_contract::journal(directory.path());
    let failures: Vec<_> = receipts(&journal)
        .into_iter()
        .filter(|r| r["reason"].is_string())
        .collect();
    assert_eq!(failures.len(), 4);
    for failure in failures {
        assert!(failure["upstream_decoded_bytes_sent"].is_null());
        assert_ne!(failure["connection_reused"], true);
    }
    assert_private(&journal);
}

#[test]
fn one_real_state_loss_recovers_once_then_continues_twenty_times() {
    for fault in [Fault::DisconnectIncremental, Fault::RejectIncremental] {
        let upstream = Upstream::with_fault(10 * 1024 * 1024, fault);
        let (directory, server, model) = server(&upstream, true);
        let mut client = connect(&server, "11111111-1111-4111-8111-111111111111");
        let first = completed(turn(&mut client, &small_request(&model, None, "window")));
        let rejected = turn(&mut client, &small_request(&model, Some(&first), "window"));
        assert_eq!(rejected["error"]["code"], "previous_response_not_found");
        client.close();
        client = connect(&server, "11111111-1111-4111-8111-111111111111");
        let mut full = small_request(&model, None, "window");
        full["input"] = json!([{"role":"user","content":"small synthetic input"},{"role":"user","content":"small synthetic input"}]);
        let mut prior = completed(turn(&mut client, &full));
        for _ in 0..20 {
            prior = completed(turn(
                &mut client,
                &small_request(&model, Some(&prior), "window"),
            ));
        }
        client.close();
        let report = server.state.backend.diagnostics.snapshot();
        assert_eq!(report["continuation_recovery"]["count"], 1);
        assert_eq!(report["continuation_recovery"]["measured_count"], 1);
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(stats.ws_connections, 2);
        assert_eq!(stats.http_requests, 0);
        assert_eq!(stats.generated_hashes.len(), 22);
        let journal = crate::tests::internal_events_contract::journal(directory.path());
        let failures: Vec<_> = receipts(&journal)
            .into_iter()
            .filter(|r| r["reason"].is_string())
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0]["reason"],
            if fault == Fault::DisconnectIncremental {
                "upstream_disconnected"
            } else {
                "upstream_recovery_required"
            }
        );
        assert_private(&journal);
    }
}

#[test]
fn prefill_rejection_unexpected_output_and_cancellation_never_replay_generation() {
    for fault in [
        Fault::RejectPrefill,
        Fault::GeneratePrefill,
        Fault::SilentPrefill,
    ] {
        let upstream = Upstream::with_fault(10 * 1024 * 1024, fault);
        let (directory, server, model) = server(&upstream, true);
        let mut client = connect(&server, "11111111-1111-4111-8111-111111111111");
        let request = json!({"type":"response.create","model":model,"input":initial_input()});
        if fault == Fault::SilentPrefill {
            client.send_json(&request).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            while upstream.traffic.lock().unwrap().warmups == 0 {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(10));
            }
            client.close();
            let deadline = Instant::now() + Duration::from_secs(3);
            while upstream.traffic.lock().unwrap().cancelled_prefills == 0 {
                assert!(
                    Instant::now() < deadline,
                    "prefill upstream was not cancelled"
                );
                thread::sleep(Duration::from_millis(10));
            }
        } else {
            let result = turn(&mut client, &request);
            if fault == Fault::RejectPrefill {
                assert_eq!(result["error"]["code"], "rate_limit_exceeded");
            } else {
                assert_eq!(result["type"], "response.failed");
                assert_ne!(
                    result["response"]["error"]["code"],
                    "previous_response_not_found"
                );
            }
            client.close();
        }
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(stats.ws_connections, 1);
        assert_eq!(stats.http_requests, 0);
        assert!(stats.generated_hashes.is_empty());
        let journal = crate::tests::internal_events_contract::journal(directory.path());
        if fault != Fault::SilentPrefill {
            assert_eq!(
                receipts(&journal)[0]["reason"],
                if fault == Fault::RejectPrefill {
                    "prefill_rejected"
                } else {
                    "prefill_invalid_response"
                }
            );
        }
        assert_private(&journal);
    }
}
