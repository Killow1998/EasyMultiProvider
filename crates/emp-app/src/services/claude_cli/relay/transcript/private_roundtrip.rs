//! Explicit opt-in acceptance. Input is stdin, never a repository fixture.
//! No transcript, image, token, hash or wire body is logged or retained.
//! The CLI's temporary system-prompt file is removed with its isolated home.
use super::*;
use crate::services::claude_cli::{cancellation::Cancellation, media, process, projection};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
#[ignore = "explicit private stdin consent and macOS egress check required; never a CI fixture"]
fn private_cpa_transcript_roundtrip() {
    assert_eq!(
        std::env::var("EMP_PRIVATE_REPLAY").as_deref(),
        Ok("1"),
        "explicit opt-in required"
    );
    let mut input = Vec::new();
    std::io::stdin()
        .take(12 * 1024 * 1024 + 1)
        .read_to_end(&mut input)
        .unwrap();
    assert!(
        input.len() <= 12 * 1024 * 1024,
        "private input exceeds bound"
    );
    let body: Value = serde_json::from_slice(&input).expect("valid private input required");
    let prepared = media::prepare_relay(&body).expect("prepare private input");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let root = tempfile::tempdir().unwrap();
    let prompt = root.path().join("system.txt");
    std::fs::write(&prompt, &prepared.system_prompt).unwrap();
    let executable = emp_codex::installed_cli::resolve_claude_cli().expect("trusted CLI required");
    let model = body["probe_model"].as_str().unwrap_or("claude-opus-5-5");
    let cli = process::command(process::CommandConfig {
        executable: &executable.executable,
        child_path: &executable.child_path,
        model,
        effort: Some("medium"),
        output_limit: None,
        prompt_file: &prompt,
        schema: &projection::proposal_schema().unwrap(),
        input_format: prepared.input_format,
        mode: process::InvocationMode::CpaRelay {
            port,
            token: "synthetic-only-token",
            home: root.path(),
        },
        config_home: root.path(),
        cache_home: root.path(),
        temp: root.path(),
        working_directory: root.path(),
    });
    // The independently checked policy allows only this fake endpoint; in
    // particular it denies other loopback ports (including local proxies).
    let policy = format!(
        "(version 1)(allow default)(deny network*)(allow network-outbound (remote ip \"localhost:{port}\"))"
    );
    let mut guarded = Command::new("/usr/bin/sandbox-exec");
    guarded
        .args(["-p", &policy])
        .arg(cli.get_program())
        .args(cli.get_args())
        .env_clear()
        .envs(
            cli.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key, value))),
        )
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    {
        use std::os::unix::process::CommandExt;
        guarded.process_group(0);
    }
    std::thread::scope(|scope| {
        let expected = &prepared.expected_user_content;
        let wire = scope.spawn(move || -> Result<usize, &'static str> {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let (mut stream,_) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline { return Err("fake upstream timeout"); }
                        std::thread::sleep(Duration::from_millis(5)); continue;
                    }
                    Err(_) => return Err("fake accept failed"),
                };
                // macOS accepted sockets inherit O_NONBLOCK. This fixture's
                // bounded HTTP reader expects blocking IO; otherwise a
                // fragmented CLI header looks like EOF and causes retries.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let raw = crate::http::request::read_request_head(&mut stream).ok_or("missing head")?;
                let head = crate::http::request::parse_request(&raw.head).ok_or("invalid head")?;
                if head.headers.starts_with("HEAD ") {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap(); continue;
                }
                let length: usize = head.header("content-length").ok_or("length required")?.parse().map_err(|_| "length invalid")?;
                if length > 12 * 1024 * 1024 || raw.body_prefix.len() > length { return Err("wire bound"); }
                let mut bytes = raw.body_prefix;
                let remaining = length - bytes.len();
                (&mut stream).take(remaining as u64).read_to_end(&mut bytes).map_err(|_| "wire read failed")?;
                let mut actual: Value = serde_json::from_slice(&bytes).map_err(|_| "wire JSON invalid")?;
                validate(&mut actual, expected)?;
                let ExpectedUserContent::Blocks(original) = expected else { return Err("blocks required"); };
                let output = actual["messages"][0]["content"].as_array().ok_or("blocks missing")?;
                let mut images = 0;
                for (source,routed) in original.iter().zip(output) {
                    if source["type"] == "image" {
                        if source["source"] != routed["source"] { return Err("source image bytes changed"); }
                        images += 1;
                    }
                }
                let events = [
                    serde_json::json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":1}}}),
                    serde_json::json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_fixture","name":"StructuredOutput","input":{}}}),
                    serde_json::json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"answer\":\"ok\",\"tool_calls\":[]}"}}),
                    serde_json::json!({"type":"content_block_stop","index":0}),
                    serde_json::json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":1}}),
                    serde_json::json!({"type":"message_stop"}),
                ];
                let payload = events.iter().map(|event| format!("event: {}\ndata: {event}\n\n",event["type"].as_str().unwrap())).collect::<String>();
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",payload.len()).unwrap();
                return Ok(images);
            }
        });
        let cancelled = Cancellation::new().unwrap();
        let result = process::run(guarded, Arc::from(prepared.stdin), &cancelled, || None);
        let wire_result = wire.join().unwrap();
        if let Err(reason) = &result {
            eprintln!(
                "PRIVATE_REPLAY_FAILURE wire_validated={} reason={reason}",
                wire_result.is_ok()
            );
        }
        // Errors here are fixed production classifications, never stdout or
        // user data. This distinguishes timeouts from transcript rejection.
        let output = result.expect("isolated CLI failed");
        let events: Vec<Value> = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("CLI JSONL required"))
            .collect();
        let terminals: Vec<_> = events
            .iter()
            .filter(|event| event["type"] == "result")
            .collect();
        assert_eq!(terminals.len(), 1, "one CLI terminal required");
        assert!(
            terminals[0]["subtype"] == "success",
            "CLI success terminal required"
        );
        assert!(
            !events
                .iter()
                .any(|event| event["type"] == "error" || event["is_error"] == true),
            "CLI error event"
        );
        let parsed =
            projection::parse_cli_result(&output).expect("valid structured result required");
        assert!(
            parsed["structured_output"] == serde_json::json!({"answer":"ok","tool_calls":[]}),
            "fake result must complete exactly"
        );
        let images = wire_result.expect("production prepare/validate roundtrip failed");
        assert_eq!(images, 3, "expected reported three-image case");
        println!("PRIVATE_REPLAY_PASS images={images}");
    });
}
