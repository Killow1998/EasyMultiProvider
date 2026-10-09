//! User journeys through the real EMP executable: browser bootstrap and
//! management, Codex integration of a temporary CODEX_HOME, model catalog,
//! Responses traffic through a loopback upstream, usage visibility, restart
//! persistence and the error surfaces a user sees.
#![cfg(unix)]

mod support;

use serde_json::{Value, json};
use support::{
    Emp, NATIVE_TOKEN, ORIGINAL_CODEX_CONFIG, PROVIDER_KEY, Upstream, Workspace, emp_command,
    write_file,
};

fn output_types(response: &Value) -> Vec<&str> {
    response["output"]
        .as_array()
        .expect("output array")
        .iter()
        .map(|item| item["type"].as_str().expect("item type"))
        .collect()
}

fn joined_deltas(events: &[Value], kind: &str) -> String {
    events
        .iter()
        .filter(|event| event["type"] == kind)
        .map(|event| event["delta"].as_str().expect("delta text"))
        .collect()
}

fn chat_usage() -> Value {
    json!({
        "prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150,
        "prompt_tokens_details": {"cached_tokens": 80},
        "completion_tokens_details": {"reasoning_tokens": 20},
    })
}

fn responses_usage() -> Value {
    json!({
        "input_tokens": 120, "output_tokens": 30, "total_tokens": 150,
        "input_tokens_details": {"cached_tokens": 80},
        "output_tokens_details": {"reasoning_tokens": 20},
    })
}

#[test]
fn cli_reports_version_and_help() {
    let version = emp_command().arg("--version").output().expect("run EMP");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("EMP {}\n", emp_app::VERSION)
    );
    for arguments in [&["--help"][..], &["serve", "--help"], &["doctor", "--help"]] {
        let help = emp_command().args(arguments).output().expect("run EMP");
        assert!(help.status.success(), "{arguments:?}");
        assert!(
            String::from_utf8_lossy(&help.stdout).contains("usage:"),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&help.stdout)
        );
    }
}

#[test]
fn browser_bootstrap_is_one_use_and_unlocks_management_without_secrets() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    assert!(
        emp.startup_output
            .iter()
            .any(|line| *line == format!("Configuration file: {}", workspace.config_path.display())),
        "{:?}",
        emp.startup_output
    );

    let page = emp.anonymous("GET", "/");
    assert_eq!(page.status, 200);
    assert_eq!(page.body, emp_app::WEB_INDEX_BYTES);

    let replay = support::http(
        emp.port,
        "POST",
        "/api/session",
        &[("X-EMP-Bootstrap", &emp.bootstrap)],
        b"",
    );
    assert_eq!(replay.status, 401, "bootstrap tokens are single use");
    assert_eq!(emp.anonymous("GET", "/api/config").status, 401);

    let config = emp.get("/api/config");
    assert_eq!(config.status, 200, "{}", config.text());
    assert!(!config.text().contains(PROVIDER_KEY));
    assert!(!config.text().contains(NATIVE_TOKEN));
    let config = config.json();
    assert_eq!(
        config["port"], emp.port,
        "publish the actual bound listener port"
    );
    assert_eq!(config["models"].as_array().expect("models").len(), 4);
    assert_eq!(config["emp_version"], emp_app::VERSION);
    upstream.assert_idle();
}

#[test]
fn support_report_is_authenticated_downloadable_and_private() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    assert_eq!(emp.anonymous("GET", "/api/support-report").status, 401);

    let response = emp.get("/api/support-report");
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(
        response.header("content-disposition"),
        Some("attachment; filename=\"EMP-support-report.json\"")
    );
    let text = response.text();
    for private in [
        PROVIDER_KEY,
        NATIVE_TOKEN,
        "fixture-account",
        workspace.root.to_str().expect("UTF-8 root"),
    ] {
        assert!(!text.contains(private), "support report leaked {private}");
    }
    let report = response.json();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["emp_version"], emp_app::VERSION);
    assert_eq!(report["configuration"]["location"], "custom");
    assert_eq!(report["configuration"]["path"], "<custom>/config.json");
    assert_eq!(report["configuration"]["exists"], true);
    assert_eq!(report["network"]["connectivity_probe"], "not_run");
    assert_eq!(report["accounts"]["imported_count"], 0);
    // The value reflects EMP's own background quota check (which depends on
    // whether a Codex binary exists on this machine), not the report.
    assert!(report["accounts"]["native"]["quota_status"].is_string());
    assert!(
        report["codex"]["inventory"]
            .as_array()
            .expect("inventory")
            .len()
            <= 16
    );
    // Producing the report must not refresh quota or contact any provider.
    upstream.assert_idle();
}

#[test]
fn second_service_for_the_same_configuration_is_rejected() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let before = std::fs::read(&workspace.config_path).expect("config");

    let contender = workspace
        .command()
        .args(workspace.serve_arguments())
        .output()
        .expect("run contender");
    assert_eq!(contender.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&contender.stderr)
            .contains("another EMP service owns this configuration"),
        "{}",
        String::from_utf8_lossy(&contender.stderr)
    );
    assert_eq!(
        std::fs::read(&workspace.config_path).expect("config"),
        before
    );
    assert_eq!(emp.anonymous("GET", "/healthz").status, 200);
}

#[test]
fn codex_sees_the_enabled_model_catalog() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let response = emp.codex("GET", "/v1/models?client_version=0.156.1", None);
    assert_eq!(response.status, 200, "{}", response.text());
    let slugs: Vec<String> = response.json()["models"]
        .as_array()
        .expect("models")
        .iter()
        .map(|model| model["slug"].as_str().expect("slug").to_owned())
        .collect();
    for id in [
        "test/model",
        "anthropic/model",
        "responses/model",
        "native/model",
    ] {
        assert!(
            slugs.iter().any(|slug| slug == id),
            "{id} missing from {slugs:?}"
        );
    }
    upstream.assert_idle();
}

#[test]
fn chat_completion_reasoning_and_usage_arrive_as_a_responses_result() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_json(&json!({
        "choices": [{"message": {"reasoning_content": "Check the sum.", "content": "Four."},
                     "finish_reason": "stop"}],
        "usage": chat_usage(),
    }));

    let response = emp.responses(&json!({
        "model": "test/model", "input": "hello", "reasoning": {"effort": "low"},
    }));
    assert_eq!(response.status, 200, "{}", response.text());
    let forwarded = upstream.next_request();
    assert_eq!(forwarded.path, "/v1/chat/completions");
    assert_eq!(
        forwarded.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {PROVIDER_KEY}").as_str())
    );
    let forwarded = forwarded.body.expect("chat body");
    assert_eq!(forwarded["model"], "upstream-model");
    upstream.assert_idle();

    let result = response.json();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["output_text"], "Four.");
    assert_eq!(result["usage"], responses_usage());
    assert_eq!(output_types(&result), ["reasoning", "message"]);
}

#[test]
fn chat_stream_reasoning_and_answer_stream_as_distinct_items() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_chat_stream(&[
        json!({"choices": [{"delta": {"content": null, "reasoning_content": "Check "}}]}),
        json!({"choices": [{"delta": {"reasoning_content": "the sum.", "content": "Four."}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    ]);

    let response = emp.responses(&json!({"model": "test/model", "input": "hello", "stream": true}));
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(
        response
            .header("content-type")
            .is_some_and(|value| value.contains("text/event-stream"))
    );
    let forwarded = upstream.next_request().body.expect("chat body");
    assert_eq!(forwarded["stream"], true);
    upstream.assert_idle();

    let events = response.sse_events();
    assert_eq!(
        joined_deltas(&events, "response.reasoning_text.delta"),
        "Check the sum."
    );
    assert_eq!(
        joined_deltas(&events, "response.output_text.delta"),
        "Four."
    );
    let completed = events.last().expect("terminal event");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["output_text"], "Four.");
    assert_eq!(
        output_types(&completed["response"]),
        ["reasoning", "message"]
    );
}

#[test]
fn chat_stream_refusal_keeps_the_usage_only_tail() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_chat_stream(&[
        json!({"choices": [{"delta": {"refusal": "I cannot "}}]}),
        json!({"choices": [{"delta": {"refusal": "assist."}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": chat_usage()}),
    ]);

    let response = emp.responses(&json!({"model": "test/model", "input": "hello", "stream": true}));
    assert_eq!(response.status, 200, "{}", response.text());
    upstream.next_request();
    upstream.assert_idle();

    let events = response.sse_events();
    assert_eq!(
        joined_deltas(&events, "response.refusal.delta"),
        "I cannot assist."
    );
    let completed = events.last().expect("terminal event");
    assert_eq!(completed["type"], "response.completed");
    assert_eq!(completed["response"]["usage"], responses_usage());
    assert_eq!(
        completed["response"]["output"][0]["content"],
        json!([{"type": "refusal", "refusal": "I cannot assist."}])
    );
}

#[test]
fn anthropic_tool_use_becomes_a_paired_function_call() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    upstream.reply_json(&json!({
        "id": "upstream-message", "type": "message", "role": "assistant",
        "content": [{"type": "text", "text": "Calculating."},
                    {"type": "tool_use", "id": "tool_fixture", "name": "calculator",
                     "input": {"x": 4}}],
        "stop_reason": "tool_use", "usage": {"input_tokens": 3, "output_tokens": 2},
    }));

    let response = emp.responses(&json!({
        "model": "anthropic/model", "input": "hello",
        "tools": [{"type": "function", "name": "calculator",
                   "parameters": {"type": "object", "properties": {"x": {"type": "number"}}}}],
    }));
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(upstream.next_request().path, "/v1/messages");
    upstream.assert_idle();

    let result = response.json();
    let call = result["output"]
        .as_array()
        .expect("output")
        .last()
        .expect("function call");
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["name"], "calculator");
    assert_eq!(call["call_id"], "tool_fixture");
    let arguments: Value =
        serde_json::from_str(call["arguments"].as_str().expect("arguments")).expect("JSON");
    assert_eq!(arguments, json!({"x": 4}));
}

#[test]
fn native_response_passes_through_unknown_fields_and_model() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let native = json!({
        "id": "upstream-response", "object": "response", "status": "completed",
        "model": "upstream-native-alias", "output": [],
        "future_native_field": {"opaque": ["retained"]},
    });
    upstream.reply_json(&native);

    let response = emp.responses(&json!({"model": "native/model", "input": "hello"}));
    assert_eq!(response.status, 200, "{}", response.text());
    let forwarded = upstream.next_request();
    assert_eq!(forwarded.path, "/v1/responses");
    // Forward-auth routes carry the caller's own Codex login upstream.
    assert_eq!(
        forwarded.headers.get("authorization").map(String::as_str),
        Some(format!("Bearer {NATIVE_TOKEN}").as_str())
    );
    upstream.assert_idle();
    assert_eq!(response.json(), native);
}

#[test]
fn upstream_503_is_visible_once_with_normalized_retry_advice() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let emp = workspace.start();
    let error = json!({"error": {"message": "temporarily unavailable", "type": "server_error"}});
    for model in ["test/model", "native/model"] {
        for advice in [
            "Thu, 01 Jan 1970 00:00:30 GMT",
            "Thursday, 01-Jan-70 00:00:30 GMT",
        ] {
            upstream.configure_with_headers(
                503,
                "application/json",
                error.to_string(),
                &[("Retry-After", advice)],
            );
            let response = emp.responses(&json!({"model": model, "input": "hello"}));
            assert_eq!(response.status, 503, "{model}: {}", response.text());
            // A date already in the past means "retry now", never a stale date.
            assert_eq!(
                response.header("retry-after"),
                Some("0"),
                "{model} {advice}"
            );
            let error = response.json()["error"].clone();
            assert_eq!(error["code"], "upstream_unavailable", "{error}");
            assert_eq!(error["retry_after_seconds"], 0, "{error}");
            upstream.next_request();
            upstream.assert_idle();
        }
    }
}

#[test]
fn codex_integration_enable_and_restore_preserve_user_toml() {
    // Instructions containing managed-looking lines are string data.
    let original = "# keep this header\n\
                    \"openai_base_url\"   = \"native\"  # keep this inline comment\n\
                    instructions = '''Read this example literally:\n\
                    openai_base_url = \"this is instruction text\"\n\
                    [example]\nend of instructions'''\n\
                    \n[nested]\nopenai_base_url = \"nested-value\"\nenabled = true\n";
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    write_file(&workspace.codex_config(), original);
    let emp = workspace.start();

    let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(enabled.status, 200, "{}", enabled.text());
    let applied = workspace.read_codex_config();
    assert!(
        applied.contains(&format!("\"http://127.0.0.1:{}/v1\"", emp.port)),
        "{applied}"
    );
    assert!(applied.contains("openai_base_url = \"this is instruction text\""));
    assert!(applied.contains("[nested]\nopenai_base_url = \"nested-value\"\nenabled = true"));
    assert!(workspace.integration_dir().join("lease.json").is_file());
    let summary = emp.get("/api/integration");
    assert_eq!(summary.status, 200, "{}", summary.text());
    assert_eq!(summary.json()["configuration"]["state"], "emp_applied");

    let restored = emp.post("/api/integration/restore", &json!({"confirm_reload": true}));
    assert_eq!(restored.status, 200, "{}", restored.text());
    assert_eq!(workspace.read_codex_config(), original);
    upstream.assert_idle();
}

#[test]
fn empty_model_picker_cannot_enable_integration() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    // Exercise a rejected manual activation from Native. With automatic
    // activation enabled, the initial non-empty catalog already owns a lease.
    let mut initial: Value =
        serde_json::from_slice(&std::fs::read(&workspace.config_path).unwrap()).unwrap();
    initial["auto_enable_on_start"] = json!(false);
    workspace.write_config(initial);
    let emp = workspace.start();
    assert_eq!(workspace.read_codex_config(), ORIGINAL_CODEX_CONFIG);
    let mut config = emp.get("/api/config").json();
    config["models"] = json!([]);
    let saved = emp.post("/api/config", &config);
    assert_eq!(saved.status, 200, "{}", saved.text());

    let response = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(response.status, 409, "{}", response.text());
    assert_eq!(response.json()["error"]["code"], "empty_emp_catalog");
    assert_eq!(workspace.read_codex_config(), ORIGINAL_CODEX_CONFIG);
    assert!(!workspace.integration_dir().join("lease.json").exists());
}

#[test]
fn subscription_search_enable_restore_and_external_edit_conflict() {
    let original = "# user preferences\nmodel = \"native\"\n[features]\nunified_exec = true\n";
    for edited in [false, true] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        write_file(&workspace.codex_config(), original);
        let emp = workspace.start();
        let mut config = emp.get("/api/config").json();
        config["subscription_search"] = json!({"enabled": true});
        let saved = emp.post("/api/config", &config);
        assert_eq!(saved.status, 200, "{}", saved.text());

        let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
        assert_eq!(enabled.status, 200, "{}", enabled.text());
        let applied = workspace.read_codex_config();
        assert!(applied.contains("web_search = \"live\""), "{applied}");
        assert!(
            applied.contains("standalone_web_search = true"),
            "{applied}"
        );
        assert!(applied.contains("unified_exec = true"), "{applied}");
        let lease_path = workspace.integration_dir().join("search.json");
        let lease: Value =
            serde_json::from_slice(&std::fs::read(&lease_path).expect("search lease"))
                .expect("lease JSON");
        assert_eq!(lease["config_path"], json!(workspace.codex_config()));

        if edited {
            let external = applied.replace("web_search = \"live\"", "web_search = \"disabled\"");
            write_file(&workspace.codex_config(), &external);
            let response = emp.post("/api/integration/restore", &json!({"confirm_reload": true}));
            assert_eq!(response.status, 409, "{}", response.text());
            assert_eq!(
                workspace.read_codex_config(),
                external,
                "a user's own edit is never overwritten"
            );
        } else {
            let response = emp.post("/api/integration/restore", &json!({"confirm_reload": true}));
            assert_eq!(response.status, 200, "{}", response.text());
            let restored = workspace.read_codex_config();
            assert!(!restored.contains("web_search"), "{restored}");
            assert!(restored.contains("unified_exec = true"), "{restored}");
            let lease: Value =
                serde_json::from_slice(&std::fs::read(&lease_path).expect("search lease"))
                    .expect("lease JSON");
            assert_eq!(lease["status"], "restored");
        }
    }
}

#[test]
fn sigterm_restores_the_owned_codex_integration() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut emp = workspace.start();
    let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(enabled.status, 200, "{}", enabled.text());
    assert!(
        workspace
            .read_codex_config()
            .contains(&format!("http://127.0.0.1:{}/v1", emp.port))
    );

    emp.signal(libc::SIGTERM);
    assert_eq!(emp.wait_exit(), Some(0));
    // The user's settings and comments come back; blank-line layout may differ.
    let content_lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    };
    let restored = workspace.read_codex_config();
    assert_eq!(
        content_lines(&restored),
        content_lines(ORIGINAL_CODEX_CONFIG),
        "{restored}"
    );
    assert!(!restored.contains("127.0.0.1"), "{restored}");
}

#[test]
fn quit_from_the_page_stops_the_process() {
    let upstream = Upstream::start();
    let workspace = Workspace::with_routes(&upstream);
    let mut emp = workspace.start();
    let enabled = emp.post("/api/integration/enable", &json!({"confirm_reload": true}));
    assert_eq!(enabled.status, 200, "{}", enabled.text());
    assert!(
        workspace
            .read_codex_config()
            .contains(&format!("http://127.0.0.1:{}/v1", emp.port))
    );
    let response = emp.post("/api/quit", &json!({}));
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.json(), json!({"status": "stopping"}));
    assert_eq!(
        emp.wait_exit_with_timeout(std::time::Duration::from_secs(30)),
        Some(0)
    );
    let restored = workspace.read_codex_config();
    let content_lines = |text: &str| -> Vec<String> {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        content_lines(&restored),
        content_lines(ORIGINAL_CODEX_CONFIG)
    );
    let mut config: Value =
        serde_json::from_slice(&std::fs::read(&workspace.config_path).unwrap()).unwrap();
    config["auto_enable_on_start"] = json!(false);
    workspace.write_config(config);
    let mut restarted = workspace.start();
    assert_eq!(restarted.get("/api/config").status, 200);
    assert_eq!(workspace.read_codex_config(), restored);
    assert_eq!(restarted.post("/api/quit", &json!({})).status, 200);
    assert_eq!(
        restarted.wait_exit_with_timeout(std::time::Duration::from_secs(30)),
        Some(0)
    );
    upstream.assert_idle();
}

fn rollout_record(kind: &str, payload: Value, second: u32) -> String {
    json!({"type": kind, "timestamp": format!("2026-09-08T01:00:{second:02}Z"),
           "payload": payload})
    .to_string()
}

#[test]
fn usage_scan_prices_codex_history_by_upstream_route() {
    let workspace = Workspace::new();
    workspace.write_config(json!({
        "providers": [{"id": "demo", "base_url": "https://example.invalid/v1",
                       "protocol": "responses", "auth_mode": "api_key"}],
        "models": [
            {"id": "demo/implicit", "provider": "demo", "upstream_id": "", "enabled": true},
            {"id": "demo/explicit", "provider": "demo", "upstream_id": "demo/actual",
             "enabled": true},
        ],
    }));
    let price = json!({"input_cost_per_token": "0.000002", "output_cost_per_token": "0.000008",
                       "cache_read_input_token_cost": "0.0000002"});
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        - 1;
    write_file(
        &workspace.root.join("state/api_prices.json"),
        &json!({"fetched_at": fetched_at,
                "prices": {"implicit": price, "actual": price}})
        .to_string(),
    );
    for (index, model) in ["demo/implicit", "demo/explicit"].iter().enumerate() {
        let turn = format!("turn-{index}");
        let last = json!({"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 20,
                          "reasoning_output_tokens": 10, "total_tokens": 120});
        let rows = [
            rollout_record(
                "session_meta",
                json!({"id": format!("session-{index}"), "model_provider": "openai",
                       "forked_from_id": null}),
                0,
            ),
            rollout_record(
                "event_msg",
                json!({"type": "task_started", "turn_id": turn}),
                0,
            ),
            rollout_record("turn_context", json!({"model": model, "turn_id": turn}), 0),
            rollout_record(
                "event_msg",
                json!({"type": "token_count", "turn_id": turn,
                       "info": {"last_token_usage": last, "total_token_usage": last}}),
                1,
            ),
        ];
        write_file(
            &workspace
                .codex_home
                .join(format!("sessions/rollout-{index}.jsonl")),
            &(rows.join("\n") + "\n"),
        );
    }
    let emp = workspace.start();

    let scan = emp.post("/api/usage/scan", &json!({}));
    assert_eq!(scan.status, 202, "{}", scan.text());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let payload = loop {
        let response = emp.get("/api/usage?start=1788825600&end=1788912000");
        assert_eq!(response.status, 200, "{}", response.text());
        let payload = response.json();
        if payload["totals"]["requests"] == 2 && payload["history"]["running"] == false {
            break payload;
        }
        assert!(std::time::Instant::now() < deadline, "{payload}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(payload["totals"]["priced_requests"], 2);
    assert_eq!(payload["totals"]["cost_nanos"], 576_000);
    let mut groups: Vec<(String, i64, i64)> = payload["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .map(|row| {
            (
                row["model"].as_str().expect("model").to_owned(),
                row["priced_requests"].as_i64().expect("priced"),
                row["cost_nanos"].as_i64().expect("cost"),
            )
        })
        .collect();
    groups.sort();
    assert_eq!(
        groups,
        [
            ("actual".to_owned(), 1, 288_000),
            ("implicit".to_owned(), 1, 288_000)
        ]
    );
}

#[test]
fn discovered_models_import_and_survive_a_restart() {
    let upstream = Upstream::start();
    let workspace = Workspace::new();
    // A pasted endpoint URL is normalized back to the provider's /v1 root.
    workspace.write_config(json!({
        "providers": [{"id": "demo", "base_url": format!("{}/responses/", upstream.base_url()),
                       "protocol": "responses", "auth_mode": "api_key",
                       "api_key": PROVIDER_KEY}],
        "models": [],
    }));
    let advertised = json!({"data": [
        {"id": "vision", "name": "Vision", "created_at": "2026-01-01",
         "architecture": {"input_modalities": ["text", "image"],
                          "output_modalities": ["text", "audio"]}},
        {"id": "audio", "name": "Audio", "created_at": "2026-09-20T00:00:00Z",
         "architecture": {"input_modalities": ["audio"]}},
    ]});
    let emp = workspace.start();

    upstream.reply_json(&advertised);
    let preview = emp.post("/api/providers/discover", &json!({"provider": "demo"}));
    assert_eq!(preview.status, 200, "{}", preview.text());
    let request = upstream.next_request();
    assert_eq!(
        (request.method.as_str(), request.path.as_str()),
        ("GET", "/v1/models")
    );
    let preview = preview.json();
    let previewed: Vec<(Value, Value, Value)> = preview["models"]
        .as_array()
        .expect("preview models")
        .iter()
        .map(|model| {
            (
                model["upstream_id"].clone(),
                model["created_at"].clone(),
                model["input_modalities"].clone(),
            )
        })
        .collect();
    // Date-only values are midnight UTC; full timestamps keep their instant.
    assert_eq!(
        previewed,
        [
            (
                json!("vision"),
                json!(1_767_225_600),
                json!(["text", "image"])
            ),
            (json!("audio"), json!(1_789_862_400), json!(["audio"])),
        ]
    );
    // Preview alone changes nothing.
    assert_eq!(emp.get("/api/config").json()["models"], json!([]));

    upstream.reply_json(&advertised);
    let imported = emp.post(
        "/api/providers/discover",
        &json!({"provider": "demo", "selected": ["vision", "audio"]}),
    );
    assert_eq!(imported.status, 200, "{}", imported.text());
    assert_eq!(imported.json()["added"], 2);
    upstream.next_request();
    emp.stop();

    let restarted = workspace.start();
    let config = restarted.get("/api/config").json();
    assert_eq!(config["providers"][0]["base_url"], upstream.base_url());
    let saved: Vec<(Value, Value)> = config["models"]
        .as_array()
        .expect("saved models")
        .iter()
        .map(|model| {
            (
                model["created_at"].clone(),
                model["input_modalities"].clone(),
            )
        })
        .collect();
    assert_eq!(
        saved,
        [
            (json!(1_767_225_600), json!(["text", "image"])),
            (json!(1_789_862_400), json!(["audio"])),
        ]
    );
    let catalog = restarted.codex("GET", "/v1/models?client_version=0.156.1", None);
    assert_eq!(catalog.status, 200, "{}", catalog.text());
    assert_eq!(
        catalog.json()["models"].as_array().expect("models").len(),
        2,
        "imported models are offered to Codex after restart"
    );
    upstream.assert_idle();
}

#[test]
fn desktop_launch_opens_the_bootstrap_url_on_the_configured_port() {
    let workspace = Workspace::new();
    let port = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve port")
        .local_addr()
        .expect("address")
        .port();
    // The per-user desktop location: XDG on Linux, Application Support on macOS.
    let config_path = if cfg!(target_os = "macos") {
        workspace
            .root
            .join("home/Library/Application Support/EasyMultiProvider/config.json")
    } else {
        workspace.root.join("xdg/easy-multi-provider/config.json")
    };
    write_file(
        &config_path,
        &json!({"host": "127.0.0.1", "port": port,
                "native_catalog_path": workspace.root.join("native.json")})
        .to_string(),
    );
    let opened = workspace.root.join("browser-url.txt");
    let browser = workspace.root.join("browser");
    write_file(
        &browser,
        // Write then rename so the test never observes a partial URL.
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > '{0}.part' && mv '{0}.part' '{0}'\n",
            opened.display()
        ),
    );
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&browser, std::fs::Permissions::from_mode(0o700))
            .expect("browser mode");
    }
    let mut command = workspace.command();
    command.env("BROWSER", &browser);
    let emp = Emp::spawn(command);
    assert_eq!(emp.port, port);
    assert_eq!(emp.anonymous("GET", "/healthz").status, 200);
    assert!(
        emp.startup_output
            .iter()
            .any(|line| *line == format!("Configuration file: {}", config_path.display())),
        "{:?}",
        emp.startup_output
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !opened.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "browser was not launched"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let url = std::fs::read_to_string(&opened).expect("opened URL");
    assert!(
        url.starts_with(&format!("http://127.0.0.1:{port}/?bootstrap=")),
        "{url}"
    );
}
