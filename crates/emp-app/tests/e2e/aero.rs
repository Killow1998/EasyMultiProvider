use crate::support::modules::{BuiltModule, ModuleRelease, eventually};
use crate::support::{Emp, NATIVE_TOKEN, Upstream, Workspace, http};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;

fn report(emp: &Emp, path: &str) -> Value {
    let response = emp.get(path);
    assert_eq!(response.status, 200, "{path}: {}", response.text());
    response.json()
}

fn response(id: &str, output: Value, input: u64, generated: u64) -> Value {
    json!({"id":id, "object":"response", "status":"completed", "model":"aero-upstream",
        "output":[output], "usage":{"input_tokens":input, "output_tokens":generated,
        "total_tokens":input+generated}})
}

fn message(id: &str, text: &str) -> Value {
    json!({"id":id, "type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text", "text":text, "annotations":[]}]})
}

fn catalog(emp: &Emp) -> Vec<Value> {
    report(emp, "/v1/models?client_version=0.169.0")["models"]
        .as_array()
        .expect("client model catalog")
        .clone()
}

#[test]
#[ignore = "needs packaged Aero; run cargo xtask module-e2e with EMP_AERO_PYTHON"]
fn m04_packaged_aero_hands_off_streams_attributes_usage_and_stops() {
    let package = PathBuf::from(std::env::var_os("EMP_E2E_AERO_PACKAGE").expect(
        "设置 EMP_AERO_PYTHON 指向装有 PyInstaller 的 Python，然后运行 cargo xtask module-e2e",
    ));
    let module = BuiltModule::from_binary(&package, "aero");
    let expected_version = include_str!("../../../../modules/aero/worker.py")
        .lines()
        .find_map(|line| line.strip_prefix("VERSION = \"")?.strip_suffix('"'))
        .expect("Aero package source version");
    assert_eq!(module.version, expected_version, "stale Aero package");
    let checksum = std::fs::read_to_string(package.with_file_name(format!(
        "{}.sha256",
        package.file_name().expect("package filename").to_string_lossy()
    )))
    .expect("package checksum");
    assert_eq!(
        checksum.split_whitespace().next(),
        Some(module.digest.as_str())
    );

    let planner = Upstream::start();
    let executor = Upstream::start();
    let plan = "Implement the requested change and verify the result";
    planner.reply_response_script(&[
        response(
            "resp-m04-plan",
            json!({"id":"fc-plan", "type":"function_call", "name":"exit_plan_mode",
                "call_id":"call-m04-plan", "arguments":json!({"plan":plan}).to_string(),
                "status":"completed"}),
            7,
            3,
        ),
        response(
            "resp-m04-review",
            message("msg-review", "M04_AERO_COMPLETE"),
            7,
            3,
        ),
    ]);
    executor.reply_response_script(&[response(
        "resp-m04-execute",
        message("msg-execute", "M04_EXECUTOR_VERIFIED"),
        11,
        5,
    )]);
    let workspace = Workspace::new();
    let mut config = json!({"codex_base_url":planner.base_url(), "auto_enable_on_start":false,
        "providers":[], "models":[]});
    for (id, upstream, model) in [
        ("planner-api", &planner, "planner-model"),
        ("executor-api", &executor, "executor-model"),
    ] {
        config["providers"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":id,
            "base_url":upstream.base_url(), "protocol":"responses", "auth_mode":"api_key",
            "api_key":format!("{id}-key")}));
        config["models"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":format!("{id}/model"),
            "provider":id, "upstream_id":model, "enabled":true}));
    }
    workspace.write_config(config);
    let release = ModuleRelease::start(std::slice::from_ref(&module));
    let emp = release.start_emp(&workspace);
    assert_eq!(emp.status("aero")["active"], false);
    emp.change("aero", "install");
    let pids = emp.active_processes(&module);
    assert!(
        release.downloads() > 0,
        "install must download the actual package"
    );
    let mut config = report(&emp, "/api/config");
    config["aero_profiles"] = json!([{"id":"aero/m04", "name":"M04 Pair",
        "planner":{"model":"planner-api/model", "effort":"low"},
        "executor":{"model":"executor-api/model", "effort":"high"}}]);
    let saved = emp.post("/api/config", &config);
    assert_eq!(saved.status, 200, "{}", saved.text());
    assert!(
        catalog(&emp)
            .iter()
            .any(|row| row["slug"] == "aero/M04 Pair")
    );

    let session = "m04-session";
    let result = http(
        emp.port,
        "POST",
        "/v1/responses",
        &[
            ("Authorization", &format!("Bearer {NATIVE_TOKEN}")),
            ("Content-Type", "application/json"),
            ("session-id", session),
        ],
        json!({"model":"aero/M04 Pair", "input":"Implement and verify the change",
            "stream":true})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(result.status, 200, "{}", result.text());
    assert!(
        result
            .header("content-type")
            .unwrap()
            .contains("text/event-stream")
    );
    let events = result.sse_events();
    let streamed: String = events
        .iter()
        .filter(|event| event["type"] == "response.output_text.delta")
        .filter_map(|event| event["delta"].as_str())
        .collect();
    for text in ["M04_EXECUTOR_VERIFIED", "M04_AERO_COMPLETE"] {
        assert!(
            streamed.contains(text),
            "missing streamed {text}: {events:?}"
        );
    }
    let completed: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "response.completed")
        .collect();
    assert_eq!(completed.len(), 1, "{events:?}");
    let final_response = &completed[0]["response"];
    assert_eq!(final_response["status"], "completed", "{final_response}");
    assert_eq!(final_response["model"], "aero/M04 Pair");
    assert_eq!(final_response["usage"]["input_tokens"], 25);
    assert_eq!(final_response["usage"]["output_tokens"], 11);
    assert!(
        final_response["output"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["phase"] == "final_answer"
                && item["content"][0]["text"] == "M04_AERO_COMPLETE")
    );
    assert!(!events.iter().any(|event| matches!(
        event["type"].as_str(),
        Some("error" | "response.failed" | "response.incomplete")
    )));
    assert!(
        !result.text().contains("exit_plan_mode"),
        "internal handoff leaked"
    );

    let calls = eventually(Duration::from_secs(5), "three completed role calls", || {
        let calls = report(&emp, "/api/calls?start=0&end=4102444800");
        let records = calls["records"].as_array().expect("call ledger records");
        (records.len() >= 3 && records.iter().all(|row| row["state"] == "completed"))
            .then_some(calls)
    });
    let records = calls["records"].as_array().expect("call ledger records");
    assert_eq!(records.len(), 3, "{calls}");
    for (upstream, provider, model, role, count, input, output) in [
        (&planner, "planner-api", "planner-model", "planner", 2, 7, 3),
        (
            &executor,
            "executor-api",
            "executor-model",
            "executor",
            1,
            11,
            5,
        ),
    ] {
        let source = format!("{provider}/model");
        let suffix = format!("{:x}", Sha256::digest(serde_json::to_vec(&source).unwrap()));
        let child_session = format!("{session}:{}", &suffix[..12]);
        let mut request_ids = Vec::new();
        for _ in 0..count {
            let request = upstream.next_forwarded();
            assert_eq!(request.method, "POST");
            assert_eq!(request.path, "/v1/responses");
            assert_eq!(
                request.headers["authorization"],
                format!("Bearer {provider}-key")
            );
            let body = request.body.expect("role request body");
            assert_eq!(body["model"], model, "{body}");
            assert_eq!(body["stream"], true);
            assert_eq!(
                body["reasoning"]["effort"],
                if role == "planner" { "low" } else { "high" },
                "{body}"
            );
            let context = body["input"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .to_string();
            assert!(context.contains(&format!("Current role: {role}")), "{body}");
            let tool = if role == "planner" {
                "exit_plan_mode"
            } else {
                "enter_plan_mode"
            };
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["name"] == tool),
                "{body}"
            );
            if role == "executor" {
                assert!(
                    context.contains(plan),
                    "executor lost the planner's plan: {body}"
                );
                assert!(
                    body["input"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item["type"] == "function_call_output"
                            && item["call_id"] == "call-m04-plan"),
                    "{body}"
                );
            }
            let request_id = &request.headers["x-emp-request-id"];
            let row = records
                .iter()
                .find(|row| row["request_id"] == *request_id)
                .expect("role request in the public ledger");
            for (field, expected) in [
                ("provider_id", provider),
                ("model_id", source.as_str()),
                ("client_kind", "aero"),
                ("caller_id", "local"),
                ("session_id", child_session.as_str()),
                ("state", "completed"),
            ] {
                assert_eq!(row[field], expected, "{row}");
            }
            assert_eq!(row["input_tokens"], input, "{row}");
            assert_eq!(row["output_tokens"], output, "{row}");
            request_ids.push(request_id.clone());
        }
        request_ids.sort();
        request_ids.dedup();
        assert_eq!(
            request_ids.len(),
            count,
            "each role call needs its own receipt"
        );
        upstream.assert_idle();
    }
    let usage = report(&emp, "/api/usage?start=0&end=4102444800");
    assert_eq!(usage["totals"]["requests"], 3, "{usage}");
    assert_eq!(usage["totals"]["input_tokens"], 25, "{usage}");
    assert_eq!(usage["totals"]["output_tokens"], 11, "{usage}");
    let mut workers = pids;
    workers.extend(emp.active_processes(&module));
    workers.sort();
    workers.dedup();
    emp.change("aero", "disable");
    for pid in workers {
        emp.assert_gone(pid);
    }
    assert_eq!(emp.status("aero")["active"], false);
    assert!(!catalog(&emp).iter().any(|row| {
        row["slug"]
            .as_str()
            .is_some_and(|slug| slug.starts_with("aero/"))
    }));
    emp.stop();
}
