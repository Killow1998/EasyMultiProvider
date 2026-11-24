//! Observable continuation, isolation, cancellation and upload contracts.
use super::*;
use sha2::{Digest, Sha256};
mod collaboration;
mod fixture;
mod inline_ids;
mod transport;
use fixture::*;

#[test]
#[ignore = "memory smoke: requires at least 1536 MiB available"]
fn native_websocket_accepts_a_full_128_mib_message_without_http_replay() {
    let maximum = emp_transport::MAX_RESPONSES_WEBSOCKET_BYTES;
    let upstream = Upstream::start(maximum);
    let (_directory, server, model) = server(&upstream, true);
    let mut client = connect(&server, "expanded-websocket-fixture");
    client.set_max_message_bytes(maximum).unwrap();
    let body = json!({"type":"response.create","model":model,"input":[
        {"role":"user","content":"x".repeat(65 * 1024 * 1024)}
    ]});
    let mut wire = body.to_string().into_bytes();
    drop(body);
    wire.resize(maximum, b' ');
    client
        .send_text(std::str::from_utf8(&wire).unwrap())
        .unwrap();
    drop(wire);
    let deadline = Instant::now() + Duration::from_secs(90);
    let previous = loop {
        assert!(
            Instant::now() < deadline,
            "large WebSocket turn did not complete"
        );
        let text = match client.poll_receive_text().unwrap() {
            emp_transport::WebSocketPoll::Text(text) => text,
            emp_transport::WebSocketPoll::Pending => continue,
            other => panic!("unexpected large WebSocket frame: {other:?}"),
        };
        let event: Value = serde_json::from_str(&text).unwrap();
        assert_ne!(event["type"], "error", "{event}");
        if event["type"] == "response.completed" {
            break event["response"]["id"].as_str().unwrap().to_owned();
        }
    };
    completed(turn(
        &mut client,
        &json!({"type":"response.create","model":model,
        "previous_response_id":previous,"input":[{"role":"user","content":"incremental follow-up"}]}),
    ));
    client.close();
    let limits = Arc::clone(&server.state.backend.transport.request_limits);
    server.shutdown().unwrap();
    assert_eq!(limits.snapshot().unwrap().reserved_bytes, 0);
    let stats = upstream.traffic.lock().unwrap();
    assert_eq!(stats.ws_connections, 1);
    assert_eq!(stats.http_requests, 0);
    assert_eq!(stats.generated_hashes.len(), 2);
    assert!(stats.application_bytes > 65 * 1024 * 1024);
}

#[test]
fn native_app_page_context_keeps_large_history_prefill_and_exact_order() {
    for imported in [false, true] {
        let upstream = Upstream::start(10 * 1024 * 1024);
        let (_directory, server, model) = server(&upstream, imported);
        let mut input = initial_input();
        for (index, name) in [
            "codex_apps_open_page",
            "codex_apps_selected_page_part_1_of_1",
        ]
        .into_iter()
        .enumerate()
        {
            input.insert(
                index + 2,
                json!({"type":"function_call_output",
                "id":format!("fco_page_context_{index}"), "namespace":"agent",
                "name":name, "output":format!("public app context {index}")}),
            );
        }
        let body = json!({"type":"response.create","model":model,"input":input});
        let mut expected = Sha256::new();
        append_input(&mut expected, &body);
        let mut client = connect(&server, "app-page-context-fixture");
        completed(turn(&mut client, &body));
        client.close();
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        assert_eq!(
            stats.generated_hashes,
            [format!("{:x}", expected.finalize())]
        );
        assert!(
            stats.warmups > 0,
            "large history must use generation-free batches"
        );
        assert_eq!(stats.rejected_large, 0);
        assert_eq!(stats.http_requests, 0);
        assert_eq!(stats.ws_connections, 1);
    }
}

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
        let journal = crate::tests::internal_events_contract::final_journal(directory.path());
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
    let journal = crate::tests::internal_events_contract::final_journal(directory.path());
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
        let journal = crate::tests::internal_events_contract::final_journal(directory.path());
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
            let started = Instant::now();
            let deadline = started + Duration::from_secs(30);
            while upstream.traffic.lock().unwrap().warmups == 0 {
                assert!(Instant::now() < deadline);
                #[expect(
                    clippy::disallowed_methods,
                    reason = "续接替身通过共享状态报告进展，以固定间隔轮询"
                )]
                thread::sleep(Duration::from_millis(10));
            }
            if started.elapsed() > Duration::from_secs(10) {
                eprintln!(
                    "large-context fixture prefill arrived after {:?}",
                    started.elapsed()
                );
            }
            client.close();
            let deadline = Instant::now() + Duration::from_secs(3);
            while upstream.traffic.lock().unwrap().cancelled_prefills == 0 {
                assert!(
                    Instant::now() < deadline,
                    "prefill upstream was not cancelled"
                );
                #[expect(
                    clippy::disallowed_methods,
                    reason = "续接替身通过共享状态报告进展，以固定间隔轮询"
                )]
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
        let journal = crate::tests::internal_events_contract::final_journal(directory.path());
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

#[cfg(unix)]
#[test]
fn prefill_disconnect_waits_for_the_client_to_retry() {
    use crate::server::raw_tcp::{RawClient, read_websocket_frame, websocket_frame};
    use tokio::io::AsyncWriteExt;

    {
        let upstream = Upstream::with_fault(10 * 1024 * 1024, Fault::SilentPrefill);
        let (_directory, server, model) = fixture::server(&upstream, true);
        let address = server.local_addr();
        let runtime = &server.state.backend.transport.runtime;
        let thread_id = "11111111-1111-4111-8111-111111111111";
        let request = json!({"type":"response.create","model":model,"input":initial_input()});
        let wire = format!(
            "GET /v1/responses HTTP/1.1\r\nHost: {}\r\nX-EMP-Session: {}\r\nthread-id: {thread_id}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            server.local_addr(),
            server.session_token(),
        );
        let frame = websocket_frame(
            0x81,
            Some([11, 12, 13, 14]),
            &serde_json::to_vec(&request).unwrap(),
        );
        let send_request = async || {
            let mut client = RawClient::request(address, wire.as_bytes()).await;
            assert!(
                client
                    .head
                    .starts_with("HTTP/1.1 101 Switching Protocols\r\n")
            );
            client.io.write_all(&frame).await.unwrap();
            client
        };
        let mut client = runtime.block_on(send_request());
        crate::tests::wait_for(Duration::from_secs(30), || {
            upstream.traffic.lock().unwrap().warmups == 1
        });
        runtime.block_on(client.io.shutdown()).unwrap();
        runtime.block_on(async {
            assert_eq!(client.rest().await.last_chunk::<2>(), Some(&[0x88, 0]));
        });
        crate::tests::wait_for(Duration::from_secs(5), || {
            upstream.traffic.lock().unwrap().cancelled_prefills == 1
        });
        {
            let stats = upstream.traffic.lock().unwrap();
            assert_eq!(stats.ws_connections, 1);
            assert_eq!(stats.http_requests, 0);
            assert!(stats.generated_hashes.is_empty());
        }
        runtime.block_on(async {
            let mut retry = send_request().await;
            loop {
                let (flags, payload) = read_websocket_frame(&mut retry.io, false).await;
                assert_eq!(flags, 0x81);
                let event: Value = serde_json::from_slice(&payload).unwrap();
                match event["type"].as_str() {
                    Some("response.completed") => break,
                    Some("response.failed" | "response.incomplete" | "error") => panic!("{event}"),
                    _ => {}
                }
            }
            retry.io.shutdown().await.unwrap();
            assert_eq!(retry.rest().await, b"\x88\x00");
        });
        server.shutdown().unwrap();
        let stats = upstream.traffic.lock().unwrap();
        let mut expected = Sha256::new();
        append_input(&mut expected, &request);
        assert_eq!(stats.ws_connections, 2);
        assert_eq!(stats.http_requests, 0);
        assert_eq!(
            stats.generated_hashes,
            [format!("{:x}", expected.finalize())]
        );
    }
}
