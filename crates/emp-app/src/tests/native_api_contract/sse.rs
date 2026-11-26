use super::support::*;
use super::*;
use crate::tests::activity_contract::next_activity_snapshot;
use crate::tests::request_observation_contract::{lock_settlement_writes, settlement_db};
use emp_test_support::sse::Frame;

const SSE_UPSTREAM_HEAD: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n";

struct NativeErrorSseUpstream {
    address: SocketAddr,
    worker: Option<JoinHandle<std::io::Result<usize>>>,
    expected_requests: usize,
}
impl NativeErrorSseUpstream {
    fn start(wire: Vec<u8>) -> Self {
        Self::start_repeated(wire, 1)
    }
    fn start_repeated(wire: Vec<u8>, requests: usize) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = thread::spawn(move || {
            for received in 0..requests {
                let (mut stream, _) = listener.accept()?;
                if Deadline::after(crate::server::raw_tcp::wire_timeout())
                    .io(&stream)
                    .peek(&mut [0])?
                    == 0
                {
                    return Ok(received);
                }
                let _ = receive_native_request(&mut stream);
                stream.write_all(SSE_UPSTREAM_HEAD)?;
                for chunk in wire.chunks(13) {
                    stream.write_all(chunk)?;
                    stream.flush()?;
                }
            }
            Ok(requests)
        });
        Self {
            address,
            worker: Some(worker),
            expected_requests: requests,
        }
    }
    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn finish(mut self) {
        let received = finish_upstream(self.address, &mut self.worker)
            .expect("finish Native SSE fake upstream")
            .expect("Native SSE worker is running");
        assert_eq!(
            received, self.expected_requests,
            "Native SSE upstream request count"
        );
    }
}
impl Drop for NativeErrorSseUpstream {
    fn drop(&mut self) {
        let _ = finish_upstream(self.address, &mut self.worker);
    }
}

fn read_native_sse(input: impl BufRead, deadline: &Deadline) -> (String, Vec<Frame>) {
    let (head, mut stream) = read_sse_head(input, deadline);
    assert!(head.starts_with("HTTP/1.1 200 "), "{head}");
    let frames = read_sse_frames(&mut stream).collect();
    deadline
        .accept((head, frames), "native SSE response")
        .unwrap()
}

fn post_native_sse(server: &ServerHandle, body: &[u8], headers: &[&str]) -> (String, Vec<Frame>) {
    let deadline = Deadline::after(response_timeout());
    let stream = open_post_stream(server, "/v1/responses", body, headers);
    read_native_sse(BufReader::new(deadline.io(stream)), &deadline)
}

fn incomplete_sse_event(frames: &[Frame]) -> Value {
    frames
        .iter()
        .filter_map(Frame::data)
        .filter_map(|data| serde_json::from_str::<Value>(&data).ok())
        .find(|event| event["type"] == "response.incomplete")
        .expect("incomplete terminal")
}

fn first_native_sse_event(frames: impl IntoIterator<Item = Frame>) -> Value {
    let frame = frames
        .into_iter()
        .find(|frame| !frame.is_comment_or_empty())
        .expect("first native SSE business frame");
    let data = frame.data().expect("native SSE event data");
    serde_json::from_str(&data).expect("native SSE event JSON")
}

#[test]
fn native_sse_activity_terminal_has_a_committed_call_and_usage() {
    assert_native_sse_activity_settlement(true);
    assert_native_sse_activity_settlement(false);
}

fn assert_native_sse_activity_settlement(subscription: bool) {
    let terminal = json!({"type":"response.completed","response":{
        "id":"resp_activity_settlement","status":"completed","model":"upstream","output":[],
        "usage":{"input_tokens":17,"output_tokens":3}
    }});
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let base_url = format!("http://{address}/v1");
    let (directory, server, model, expected_account) = if subscription {
        let (directory, server, _) = super::http::account_server(&base_url, "missing-test-codex");
        (directory, server, "demo/upstream", "selected-owner")
    } else {
        let (directory, server) = native_alias_server(&base_url);
        (directory, server, "native/alias", "caller-owner")
    };
    let root = canonical_root(&directory);
    let mut db = settlement_db(&root, &server);
    let session = session_header(&server);
    let authorization = if subscription {
        format!("Authorization: Bearer {}", server.state.backend.gateway_key)
    } else {
        "Authorization: Bearer caller".to_owned()
    };
    let body = json!({"model":model,"input":"hello","stream":true}).to_string();
    let thread_id = "activity-settlement";
    let thread_header = format!("thread-id: {thread_id}");
    let mut headers = vec![
        authorization.as_str(),
        "chatgpt-account-id: caller-owner",
        &thread_header,
    ];
    if !subscription {
        headers.push(&session);
    }
    let socket = open_post_stream(&server, "/v1/responses", body.as_bytes(), &headers);
    let deadline = Deadline::after(response_timeout());
    let mut upstream = accept_before(&listener, &deadline);
    let observed = receive_native_request(&mut upstream);
    let request_id = &observed.headers["x-emp-request-id"];
    assert_eq!(observed.headers["chatgpt-account-id"], expected_account);

    db.busy_timeout(Duration::ZERO).unwrap();
    let write_lock = lock_settlement_writes(&mut db);
    let reader = rusqlite::Connection::open(root.join("state/usage.sqlite3")).unwrap();
    let commit_lock = reader.unchecked_transaction().unwrap();
    let receipt_exists = |connection: &rusqlite::Connection| {
        connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM call_records WHERE request_id=?)",
            [request_id],
            |row| row.get::<_, bool>(0),
        )
    };
    assert!(!receipt_exists(&commit_lock).unwrap());
    let mut upstream = deadline.io(upstream);
    upstream.write_all(SSE_UPSTREAM_HEAD).unwrap();
    write!(upstream, "data: {terminal}\n\n").unwrap();
    write_lock.rollback().unwrap();

    wait_for(&deadline, || {
        receipt_exists(&db).is_err_and(|error| {
            error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
        })
    });
    let mut events = subscribe_quota_events(&server, &session);
    let matching_request = |row: &&Value| row["request_id"] == request_id.as_str();
    let blocked = next_activity_snapshot(&mut events, |_| true);
    let activity = blocked["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(matching_request)
        .unwrap();
    assert_eq!(
        activity["state"], "active",
        "activity ended while the SQLite commit was blocked"
    );
    commit_lock.commit().unwrap();

    let snapshot = next_activity_snapshot(&mut events, |value| {
        value["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(matching_request)
            .is_some_and(|row| row["state"] != "active")
    });
    let activity = snapshot["requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(matching_request)
        .unwrap();
    let (state, facts, usage_id, owner, input, output) = db
        .query_row(
            "SELECT c.state, c.facts, u.id, u.owner, u.input_tokens, u.output_tokens
         FROM call_records c JOIN usage_events u ON u.id=json_extract(c.facts,'$.usage_event_id')
         WHERE c.request_id=? AND c.operation='responses'",
            [request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .expect("activity terminal must follow the commit of the call and its usage");
    let call: Value = serde_json::from_str(&facts).unwrap();
    assert_eq!(activity["state"], "completed");
    assert_eq!(activity["state"], state);
    assert_eq!(activity["thread_id"], thread_id);
    if subscription {
        assert_eq!(activity["account_id"], call["account_id"]);
    } else {
        assert_eq!(activity["provider_id"], "native");
        assert_eq!(activity["provider_id"], call["provider_id"]);
    }
    let expected_owner = emp_state::usage::account_owner(&observed.headers);
    let expected_usage_id = emp_state::usage::identity_hash(&json!([
        if subscription {
            "subscription"
        } else {
            "native"
        },
        expected_owner,
        "upstream",
        "responses",
        terminal["response"]["id"]
    ]));
    assert_eq!(
        owner, expected_owner,
        "usage owner must match the upstream credential"
    );
    assert_eq!((input, output), (17, 3));
    assert_eq!(call["input_tokens"], input);
    assert_eq!(call["output_tokens"], output);
    assert_eq!(activity["input_tokens"], input);
    assert_eq!(activity["output_tokens"], output);
    assert_eq!(call["usage_event_id"], usage_id);
    assert_eq!(
        usage_id, expected_usage_id,
        "usage ID must include the upstream owner"
    );
    assert_eq!(call["usage_response_id"], terminal["response"]["id"]);

    let deadline = Deadline::after(response_timeout());
    let (_, frames) = read_native_sse(BufReader::new(deadline.io(socket)), &deadline);
    let completed = first_native_sse_event(frames);
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["id"], terminal["response"]["id"]);
    server.shutdown().unwrap();
}

#[test]
fn native_sse_prewarm_preserves_activity_and_billing_boundaries() {
    enum End {
        Success,
        CancelAfterOpening,
        UpstreamFailure,
        DisconnectBeforeHeaders,
    }

    for end in [
        End::Success,
        End::CancelAfterOpening,
        End::UpstreamFailure,
        End::DisconnectBeforeHeaders,
    ] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (directory, server) = native_alias_server(&format!("http://{address}/v1"));
        let cancelled = matches!(end, End::DisconnectBeforeHeaders);
        assert_prewarm_outcome(
            &directory,
            server,
            "native/alias",
            json!("hello"),
            cancelled,
            |downstream, deadline| {
                let mut upstream = accept_before(&listener, deadline);
                let observed = receive_native_request(&mut upstream);
                assert_eq!(observed.body["generate"], false);
                let mut wire = deadline.io(&upstream);

                match end {
                    End::Success => {
                        wire.write_all(SSE_UPSTREAM_HEAD).unwrap();
                        let terminal = json!({"type":"response.completed","response":{
                            "id":"resp_prewarm","status":"completed","output":[],
                            "usage":{"input_tokens":17,"output_tokens":3}
                        }});
                        write!(wire, "data: {terminal}\n\n").unwrap();
                        let (_, frames) =
                            read_native_sse(BufReader::new(deadline.io(downstream)), deadline);
                        assert_eq!(first_native_sse_event(frames)["type"], "response.completed");
                    }
                    End::UpstreamFailure => {
                        wire.write_all(SSE_UPSTREAM_HEAD).unwrap();
                        let terminal = json!({"type":"response.failed","response":{
                            "status":"failed","error":{"status":400,"code":"invalid_request"}
                        }});
                        write!(wire, "data: {terminal}\n\n").unwrap();
                        let response =
                            complete_response_from(&mut deadline.io(downstream), deadline);
                        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
                    }
                    End::CancelAfterOpening => {
                        wire.write_all(SSE_UPSTREAM_HEAD).unwrap();
                        wire.write_all(
                        b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
                    )
                    .unwrap();
                        let (_, mut stream) =
                            read_sse_head(BufReader::new(deadline.io(downstream)), deadline);
                        assert_eq!(
                            first_native_sse_event(read_sse_frames(&mut stream))["type"],
                            "response.output_text.delta"
                        );
                        drop(stream);
                        assert!(upstream_closed_before(&upstream, deadline));
                    }
                    End::DisconnectBeforeHeaders => {
                        drop(downstream);
                        assert!(upstream_closed_before(&upstream, deadline));
                    }
                }
                observed.headers["x-emp-request-id"].clone()
            },
        );
    }
}

#[test]
fn external_sse_prewarm_disconnect_publishes_cancellation_without_billing() {
    for retry in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = canonical_root(&directory);
        let server = prewarm_provider_server(
            &root,
            json!({
                "id":"demo","base_url":format!("http://{address}/v1"),
                "protocol":"responses","auth_mode":"api_key","api_key":"fixture"
            }),
        );
        assert_prewarm_outcome(
            &directory,
            server,
            "demo/model",
            json!("hello"),
            true,
            |downstream, deadline| {
                let mut upstream = accept_before(&listener, deadline);
                let observed = receive_native_request(&mut upstream);
                let request_id = observed.headers["x-emp-request-id"].clone();
                if retry {
                    let response = b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 120\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    deadline.io(&upstream).write_all(response).unwrap();
                    wait_for(deadline, || {
                        crate::tests::internal_events_contract::live_journal_until(&root, deadline)
                            .iter()
                            .any(|record| {
                                record["event"] == "model_retry_scheduled"
                                    && record["fields"]["request_id"] == request_id
                                    && record["fields"]["delay_ms"] == 120_000
                            })
                    });
                }
                drop(downstream);
                if !retry {
                    assert!(upstream_closed_before(&upstream, deadline));
                }
                request_id
            },
        );
    }
}

#[cfg(unix)]
#[test]
fn claude_cli_sse_prewarm_disconnect_publishes_cancellation_without_billing() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    const FIXTURE: &str = "EMP_TEST_PREWARM_CLAUDE";
    if let Ok(case) = std::env::var(FIXTURE) {
        let executable = PathBuf::from(std::env::var_os("PATH").unwrap()).join("claude");
        assert_eq!(
            emp_codex::installed_cli::resolve_claude_cli()
                .unwrap()
                .executable,
            executable
        );
        let input = match case.as_str() {
            "plain" => json!("hello"),
            "compaction" => json!([
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]},
                {"type":"compaction_trigger"}
            ]),
            _ => panic!("unknown Claude prewarm case: {case}"),
        };
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = canonical_root(&directory);
        let server = prewarm_provider_server(
            &root,
            json!({
                "id":"demo","base_url":"http://127.0.0.1:1/v1",
                "protocol":"anthropic_messages","auth_mode":"anthropic_api_key","api_key":"fixture",
                "execution_backend":"claude_cli"
            }),
        );
        assert_prewarm_outcome(
            &directory,
            server,
            "demo/model",
            input,
            true,
            |downstream, deadline| {
                let ready = executable.with_extension("ready");
                wait_for(deadline, || ready.is_file());
                let mut request_id = None;
                wait_for(deadline, || {
                    request_id =
                        crate::tests::internal_events_contract::live_journal_until(&root, deadline)
                            .iter()
                            .find(|record| record["event"] == "model_prewarm_started")
                            .and_then(|record| record["fields"]["request_id"].as_str())
                            .map(str::to_owned);
                    request_id.is_some()
                });
                drop(downstream);
                request_id.unwrap()
            },
        );
        return;
    }

    for case in ["plain", "compaction"] {
        let directory = emp_test_support::fixture_dir().unwrap();
        let root = canonical_root(&directory);
        let executable = root.join("claude");
        std::fs::write(
            &executable,
            b"#!/bin/sh\n: > \"$0.ready\"\nwhile IFS= read -r line; do :; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "tests::native_api_contract::sse::claude_cli_sse_prewarm_disconnect_publishes_cancellation_without_billing",
        ]).env(FIXTURE, case).env("PATH", &root);
        let output = emp_test_support::OwnedChild::output(
            command,
            Deadline::after(response_timeout() + response_timeout()),
        )
        .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{case}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn prewarm_provider_server(root: &Path, provider: Value) -> ServerHandle {
    let config = root.join("config.json");
    let native = root.join("codex/auth.json");
    std::fs::create_dir_all(native.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        json!({
            "providers":[provider],
            "models":[{"id":"demo/model","provider":"demo","upstream_id":"upstream","enabled":true}]
        })
        .to_string(),
    )
    .unwrap();
    ServerHandle::start_with_config_options(
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        0,
        &config,
        "missing-test-codex",
        native,
    )
    .unwrap()
}

fn assert_prewarm_outcome(
    directory: &TempDir,
    server: ServerHandle,
    model: &str,
    input: Value,
    cancelled: bool,
    exercise: impl FnOnce(TcpStream, &Deadline) -> String,
) {
    let db = settlement_db(&canonical_root(directory), &server);
    let session = session_header(&server);
    let publications = server.state.backend.activity.record_publications();
    let deadline = Deadline::after(response_timeout());
    let body = json!({"model":model,"input":input,"generate":false,"stream":true}).to_string();
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        body.as_bytes(),
        &[&session, "Authorization: Bearer caller"],
    );
    let request_id = exercise(downstream, &deadline);

    wait_for(&deadline, || {
        let snapshot = publications.latest();
        snapshot["routes"][0]["in_flight"] == 0
            && snapshot["routes"][0]["last_finished"].is_number()
    });
    let terminal_ids = [request_id.as_str()];
    publications.assert_intervals(if cancelled { &terminal_ids } else { &[] }, false);
    let finished = publications.latest();
    let requests = finished["requests"].as_array().unwrap();
    assert_eq!(
        requests.len(),
        usize::from(cancelled),
        "{model} prewarm activity after route release: {requests:?}"
    );
    if cancelled {
        let activity = &requests[0];
        assert_eq!(activity["request_id"], request_id);
        assert_eq!(activity["model_id"], model);
        assert_eq!(activity["state"], "cancelled");
        assert_eq!(activity["error_class"], "client_disconnect");
        assert_eq!(activity["error_origin"], "client");
        assert_eq!(activity["duration_ms"], Value::Null);
    }
    let counts = db
        .query_row(
            "SELECT (SELECT COUNT(*) FROM call_records), (SELECT COUNT(*) FROM usage_events)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap();
    assert_eq!(
        counts,
        (0, 0),
        "prewarm must not create a call receipt or usage row"
    );
    server.shutdown().unwrap();
}

#[test]
fn native_sse_finds_terminal_json_split_across_http_chunks() {
    let terminal =
        br#"data: {"type":"response.incomplete","response":{"usage":{"input_tokens":17}}}"#;
    let mut wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    let mut event = terminal.to_vec();
    event.extend_from_slice(b"\n\n");
    for chunk in event.chunks(13) {
        write!(&mut wire, "{:x}\r\n", chunk.len()).unwrap();
        wire.extend_from_slice(chunk);
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"0\r\n\r\n");
    let (_, frames) = read_native_sse(
        std::io::Cursor::new(wire),
        &Deadline::after(response_timeout()),
    );
    let decoded = incomplete_sse_event(&frames);
    assert_eq!(decoded["response"]["usage"]["input_tokens"], 17);
}

#[test]
fn native_sse_incomplete_preserves_usage_and_deduplicates_response_id() {
    for usage in [
        json!({"input_tokens":17,"output_tokens":3,"input_tokens_details":{"cached_tokens":5}}),
        Value::Null,
        json!({"input_tokens":"invalid","output_tokens":-1}),
    ] {
        let terminal = json!({"type":"response.incomplete","response":{
            "id":"resp_incomplete_usage", "object":"response", "status":"incomplete",
            "model":"upstream", "output":[], "usage":usage,
            "incomplete_details":{"reason":"max_output_tokens"}
        }});
        let wire = format!("data: {terminal}\n\n").into_bytes();
        let upstream = NativeErrorSseUpstream::start_repeated(wire, 2);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"fixture","stream":true}))
                .unwrap();
        for _ in 0..2 {
            let (_, frames) =
                post_native_sse(&server, &body, &[&cookie, "Authorization: Bearer caller"]);
            let event = incomplete_sse_event(&frames);
            assert_eq!(event["response"]["id"], terminal["response"]["id"]);
            assert_eq!(event["response"]["usage"], usage);
            assert_eq!(
                event["response"]["incomplete_details"],
                terminal["response"]["incomplete_details"]
            );
        }
        let now = crate::util::system_now();
        let totals = server
            .state
            .backend
            .usage
            .ledger
            .query(0.0, now, "all", now, None)
            .unwrap();
        assert_eq!(
            totals["totals"]["input_tokens"].as_u64().unwrap_or(0),
            usage["input_tokens"].as_u64().unwrap_or(0),
            "{totals}"
        );
        assert_eq!(
            totals["totals"]["output_tokens"].as_u64().unwrap_or(0),
            usage["output_tokens"].as_u64().unwrap_or(0),
            "{totals}"
        );
        upstream.finish();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_and_ordinary_json_cross_the_real_endpoint() {
    for ordinary in [false, true] {
        let upstream = NativeSseUpstream::start(ordinary);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let (head, frames) = post_native_sse(
            &server,
            &body,
            &[
                &cookie,
                "Authorization: Bearer caller",
                "thread-id: stream-thread",
                "x-openai-subagent: stream-subagent",
            ],
        );
        let has_event = |value| frames.iter().any(|frame| frame.raw().contains(value));
        assert_eq!(
            response_header(&head, "openai-model"),
            Some("native/alias"),
            "{head}"
        );
        if !ordinary {
            assert_eq!(
                response_header(&head, "x-codex-turn-state"),
                Some("stream-turn"),
                "{head}"
            );
            assert!(!head.contains("stale-stream-etag"));
            assert!(has_event("response.created"));
            assert!(has_event("response.output_text.delta"));
            assert!(has_event("response.completed"));
            assert!(!has_event("not-json"));
            assert!(has_event("\"openai-model\":\"native/alias\""));
            assert!(has_event("\"x-openai-model\":\"native/alias\""));
            assert!(has_event("\"model\":\"upstream\""));
            assert!(has_event("\"future\":{\"opaque\":true}"));
        } else {
            assert!(has_event("response.created"));
            assert!(has_event("response.completed"));
            assert!(has_event("\"future\":\"kept\""));
        }
        let observed = upstream.observed();
        assert_eq!(observed.headers["content-encoding"], "zstd");
        assert_eq!(observed.headers["authorization"], "Bearer caller");
        assert_eq!(observed.headers["thread-id"], "stream-thread");
        assert_eq!(observed.headers["x-openai-subagent"], "stream-subagent");
        assert_eq!(observed.body["model"], "upstream");
        assert_eq!(observed.body["stream"], true);
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_context_and_incomplete_boundaries_match_codex_http_behavior() {
    let cases=[
        ("context",b"data: {\"type\":\"error\",\"error\":{\"code\":\"context_length_exceeded\",\"message\":\"maximum context length exceeded\"}}\n\n".to_vec(),413,false),
        ("pre_incomplete",b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_x\",\"status\":\"in_progress\"}}\n\n".to_vec(),502,false),
        ("post_incomplete",b"data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_x\",\"status\":\"in_progress\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".to_vec(),200,true),
        ("failed",b"data: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp_x\",\"status\":\"failed\",\"error\":{\"status\":429,\"error_class\":\"rate_limit\",\"code\":\"rate_limit_exceeded\",\"retry_after_seconds\":3}}}\n\n".to_vec(),429,false),
    ];
    for (name, wire, expected, streamed) in cases {
        let upstream = NativeErrorSseUpstream::start(wire);
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let response = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&cookie, "Authorization: Bearer caller"],
        );
        let status: u16 = response.split_whitespace().nth(1).unwrap().parse().unwrap();
        assert_eq!(status, expected, "{name}: {response}");
        if streamed {
            let (_, frames) =
                read_native_sse(response.as_bytes(), &Deadline::after(response_timeout()));
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.raw().contains("response.output_text.delta"))
            );
            assert!(frames.iter().any(|frame| {
                frame.raw().contains("response.failed") && frame.raw().contains("stream_incomplete")
            }));
        }
        if name == "failed" {
            assert_eq!(
                response_header(response_parts(&response).0, "retry-after"),
                Some("3"),
                "{response}"
            );
        }
        upstream.finish();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_retry_headers_survive_pre_output_http_conversion() {
    for (headers, legacy, expected) in [
        (json!({"Retry-After":"3.1"}), None, 4),
        (json!({"rEtRy-AfTeR":"7"}), Some(3), 7),
        (json!({"Retry-After":"invalid"}), Some(3), 3),
    ] {
        let mut error = json!({"status":503,"error_class":"upstream_5xx","headers":headers});
        if let Some(delay) = legacy {
            error["retry_after_seconds"] = json!(delay);
        }
        let event = json!({"type":"response.failed","response":{"status":"failed","error":error}});
        let upstream = NativeErrorSseUpstream::start(format!("data: {event}\n\n").into_bytes());
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let cookie = session_header(&server);
        let body =
            serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true}))
                .unwrap();
        let response = post_stream(
            &server,
            "/v1/responses",
            &body,
            &[&cookie, "Authorization: Bearer caller"],
        );
        assert!(response.starts_with("HTTP/1.1 503"), "{response}");
        assert!(
            response.contains(&format!("Retry-After: {expected}\r\n")),
            "{response}"
        );
        upstream.finish();
        server.shutdown().unwrap();
    }
}

#[test]
fn native_sse_downstream_disconnect_cancels_open_before_headers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let observation = Observation::spawn(move |probe| {
        let Some(dispatch) = probe.next_request() else {
            return;
        };
        let mut stream = accept_before(&listener, dispatch.deadline());
        let _ = receive_native_request(&mut stream);
        dispatch.complete(true);
        probe.observe(|deadline| upstream_closed_before(&stream, deadline));
    });
    let (_directory, server) = native_alias_server(&format!("http://{address}/v1"));
    let cookie = session_header(&server);
    let body = serde_json::to_vec(&json!({
        "model":"native/alias", "input":"hello", "stream":true
    }))
    .unwrap();
    let dispatched = observation.begin(
        Deadline::after(test_timeout(Duration::from_secs(2))),
        "upstream request ready",
    );
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    assert!(dispatched.wait().unwrap());
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    let cancellation = observation.begin(deadline, "upstream cancellation");
    drop(downstream);
    assert!(
        cancellation.wait().unwrap(),
        "Rust EMP retained native upstream while open_stream awaited response headers"
    );
    server.shutdown().unwrap();
    drop(observation);
}

#[test]
fn native_sse_downstream_disconnect_cancels_upstream() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let observation = Observation::spawn(move |probe| {
        let Some(dispatch) = probe.next_request() else {
            return;
        };
        let mut stream = accept_before(&listener, dispatch.deadline());
        let _ = receive_native_request(&mut stream);
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream.flush().unwrap();
        dispatch.complete(true);
        probe.observe(|deadline| upstream_closed_before(&stream, deadline));
    });
    let (_directory, server) = native_alias_server(&format!("http://{address}/v1"));
    let cookie = session_header(&server);
    let body =
        serde_json::to_vec(&json!({"model":"native/alias","input":"hello","stream":true})).unwrap();
    let dispatched = observation.begin(
        Deadline::after(test_timeout(Duration::from_secs(2))),
        "upstream request ready",
    );
    let downstream = open_post_stream(
        &server,
        "/v1/responses",
        &body,
        &[&cookie, "Authorization: Bearer caller"],
    );
    assert!(dispatched.wait().unwrap());
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    let cancellation = observation.begin(deadline, "upstream cancellation");
    drop(downstream);
    assert!(
        cancellation.wait().unwrap(),
        "Rust EMP retained native upstream after Codex disconnected"
    );
    server.shutdown().unwrap();
    drop(observation);
}

#[test]
fn responses_sse_filters_codex_extensions_only_for_generic_clients() {
    for codex in [false, true] {
        let wire = [
            json!({"type":"codex.rate_limits","rate_limits":{}}),
            json!({"type":"codex.response.metadata","headers":{}}),
            json!({"type":"response.metadata","headers":{}}),
            json!({"type":"keepalive"}),
            json!({"type":"response.output_text.delta","delta":"hello"}),
            json!({"type":"codex.rate_limits","rate_limits":{"after_output":true}}),
            json!({"type":"response.completed","response":{"id":"extension-test","status":"completed","output":[]}}),
        ].iter().map(|v| format!("data: {v}\n\n")).collect::<String>();
        let upstream = NativeErrorSseUpstream::start(wire.into_bytes());
        let (_directory, server) = native_alias_server(&upstream.base_url());
        let (_, frames) = post_native_sse(
            &server,
            br#"{"model":"native/alias","input":"hello","stream":true}"#,
            &[
                &session_header(&server),
                "Authorization: Bearer fixture",
                if codex {
                    "originator: codex_cli_rs"
                } else {
                    "User-Agent: async-openai"
                },
            ],
        );
        let has_event = |value| frames.iter().any(|frame| frame.raw().contains(value));
        assert!(has_event("response.output_text.delta"), "{frames:?}");
        assert!(has_event("response.completed"), "{frames:?}");
        for extension in [
            "codex.rate_limits",
            "codex.response.metadata",
            "response.metadata",
            "\"type\":\"keepalive\"",
        ] {
            assert_eq!(has_event(extension), codex, "{frames:?}");
        }
        if !codex {
            assert!(frames.iter().any(|frame| frame.raw() == ": keepalive\n\n"));
        }
        upstream.finish();
        server.shutdown().unwrap();
    }
}
