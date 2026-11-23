use crate::support::{Emp, Upstream, Workspace, import_account, write_file};
use serde_json::{Value, json};

fn report(emp: &Emp, path: &str) -> Value {
    let response = emp.get(path);
    assert_eq!(response.status, 200, "{path}: {}", response.text());
    response.json()
}

fn scan_report(emp: &Emp, path: &str) -> Value {
    let response = emp.post("/api/usage/scan", &json!({}));
    assert_eq!(response.status, 202, "{}", response.text());
    let command = response.json()["command_id"]
        .as_u64()
        .expect("scan command");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let payload = report(emp, path);
        if payload["history"]["completed"].as_u64().unwrap_or(0) >= command {
            assert_eq!(payload["history"]["errors"], 0, "{payload}");
            return payload;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "scan {command}: {payload}"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "用量扫描结果通过 CLI 状态提供，以固定间隔轮询"
        )]
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
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
    let emp = workspace.start_without_path_cli();

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
        #[expect(
            clippy::disallowed_methods,
            reason = "计价结果通过管理接口提供，以固定间隔轮询"
        )]
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
    let path = "/api/usage?start=1788825600&end=1788912000";
    for duplicate_files in [false, true] {
        if duplicate_files {
            for index in 0..2 {
                std::fs::copy(
                    workspace
                        .codex_home
                        .join(format!("sessions/rollout-{index}.jsonl")),
                    workspace
                        .codex_home
                        .join(format!("sessions/rollout-copy-{index}.jsonl")),
                )
                .expect("copy the same usage into a new scan source");
            }
        }
        let rescanned = scan_report(&emp, path);
        for field in ["totals", "groups", "issues"] {
            assert_eq!(
                rescanned[field], payload[field],
                "duplicate files={duplicate_files}: {field}"
            );
        }
    }
    emp.stop();
    let restarted = workspace.start_without_path_cli();
    let persisted = scan_report(&restarted, path);
    for field in ["totals", "groups", "issues"] {
        assert_eq!(persisted[field], payload[field], "after restart: {field}");
    }
}

fn seed_prices(workspace: &Workspace) {
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        - 1;
    write_file(
        &workspace.root.join("state/api_prices.json"),
        &json!({
        "fetched_at":fetched_at, "prices":{"billing-model":{
            "input_cost_per_token":"0.000002", "output_cost_per_token":"0.000008"}}})
        .to_string(),
    );
}

fn billing_snapshot(emp: &Emp) -> Value {
    let calls = report(emp, "/api/calls?start=0&end=4102444800");
    let usage = report(emp, "/api/usage?start=0&end=4102444800");
    let sessions = report(emp, "/api/usage?sessions=true&start=0&end=4102444800");
    let mut records: Vec<_> = calls["records"]
        .as_array()
        .expect("call records")
        .iter()
        .map(|row| {
            json!({"request_id":row["request_id"], "provider_id":row["provider_id"],
            "account_id":row["account_id"], "session_id":row["session_id"],
            "input_tokens":row["input_tokens"], "output_tokens":row["output_tokens"],
            "pricing":row["pricing"]})
        })
        .collect();
    records.sort_by_key(|row| row["request_id"].as_str().unwrap().to_owned());
    let mut bills: Vec<_> = sessions["records"]
        .as_array()
        .expect("session bills")
        .iter()
        .map(|row| {
            json!({"session_id":row["session_id"], "requests":row["requests"],
            "reported_requests":row["reported_requests"], "input_tokens":row["input_tokens"],
            "output_tokens":row["output_tokens"], "cost_nanos":row["cost_nanos"],
            "priced_requests":row["priced_requests"], "unpriced_requests":row["unpriced_requests"]})
        })
        .collect();
    bills.sort_by_key(|row| row["session_id"].as_str().unwrap().to_owned());
    json!({"calls":records, "call_count":calls["total"], "totals":usage["totals"],
        "groups":usage["groups"], "issues":usage["issues"], "bills":bills,
        "session_totals":sessions["totals"], "unassigned":sessions["unassigned"]})
}

#[test]
fn reports_keep_account_service_session_and_unknown_usage_after_restart() {
    let subscriptions = Upstream::start();
    let known_api = Upstream::start();
    let unknown_api = Upstream::start();
    for (upstream, id, counts) in [
        (&subscriptions, "resp_subscription_usage", Some((7, 3))),
        (&known_api, "resp_api_usage", Some((11, 5))),
        (&unknown_api, "resp_unknown_usage", None),
    ] {
        let mut response = json!({"id":id, "object":"response", "status":"completed",
            "model":"billing-model", "output":[]});
        if let Some((input, output)) = counts {
            response["usage"] = json!({"input_tokens":input, "output_tokens":output,
                "total_tokens":input+output});
        }
        upstream.reply_json(&response);
    }
    let workspace = Workspace::new();
    workspace.write_config(json!({"codex_base_url":subscriptions.base_url(),
        "account_store_path":workspace.root.join("accounts"),
        "providers":[
            {"id":"known-api", "name":"Known API", "base_url":known_api.base_url(),
             "protocol":"responses", "auth_mode":"api_key", "api_key":"known-api-key"},
            {"id":"unknown-api", "name":"Unknown API", "base_url":unknown_api.base_url(),
             "protocol":"responses", "auth_mode":"api_key", "api_key":"unknown-api-key"}],
        "models":[
            {"id":"known-api/model", "provider":"known-api", "upstream_id":"billing-model", "enabled":true},
            {"id":"unknown-api/model", "provider":"unknown-api", "upstream_id":"billing-model", "enabled":true}]}));
    seed_prices(&workspace);
    let emp = workspace.start_without_path_cli();
    for (id, token, owner) in [
        ("account-a", "account-a-token", "owner-a"),
        ("account-b", "account-b-token", "owner-b"),
    ] {
        import_account(&emp, id, token, owner);
    }
    let variants = [
        (
            "account-a/billing-model",
            "account-a",
            Some("account-a"),
            "session-a",
            &subscriptions,
            "account-a-token",
            Some("owner-a"),
            Some((7, 3)),
        ),
        (
            "account-b/billing-model",
            "account-b",
            Some("account-b"),
            "session-b",
            &subscriptions,
            "account-b-token",
            Some("owner-b"),
            Some((7, 3)),
        ),
        (
            "known-api/model",
            "known-api",
            None,
            "session-api",
            &known_api,
            "known-api-key",
            None,
            Some((11, 5)),
        ),
        (
            "unknown-api/model",
            "unknown-api",
            None,
            "session-unknown",
            &unknown_api,
            "unknown-api-key",
            None,
            None,
        ),
    ];
    let mut requests = Vec::new();
    for (route, _, _, session, upstream, token, owner, _) in &variants {
        let response = emp.responses(&json!({"model":route, "input":"usage fixture",
            "metadata":{"session_id":session}}));
        assert_eq!(response.status, 200, "{route}: {}", response.text());
        let forwarded = upstream.next_forwarded();
        assert_eq!(forwarded.path, "/v1/responses");
        assert_eq!(
            forwarded.headers["authorization"],
            format!("Bearer {token}")
        );
        assert_eq!(
            forwarded
                .headers
                .get("chatgpt-account-id")
                .map(String::as_str),
            *owner
        );
        assert_eq!(forwarded.body.as_ref().unwrap()["model"], "billing-model");
        requests.push(forwarded.headers["x-emp-request-id"].clone());
    }
    let calls = report(&emp, "/api/calls?start=0&end=4102444800");
    assert_eq!(calls["total"], 4);
    for ((_, provider, account, session, _, _, _, counts), request) in
        variants.iter().zip(&requests)
    {
        let row = calls["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["request_id"] == *request)
            .expect("forwarded request in call report");
        assert_eq!(row["provider_id"], *provider, "{row}");
        assert_eq!(row["account_id"].as_str(), *account, "{row}");
        assert_eq!(row["session_id"], *session, "{row}");
        assert_eq!(
            row["input_tokens"],
            counts.map_or(Value::Null, |(input, _)| json!(input))
        );
        assert_eq!(
            row["output_tokens"],
            counts.map_or(Value::Null, |(_, output)| json!(output))
        );
        let filter = account.map_or_else(
            || format!("provider={provider}"),
            |id| format!("account_id={id}"),
        );
        let scoped = report(
            &emp,
            &format!("/api/usage?series=true&start=0&end=4102444800&{filter}&session={session}"),
        );
        assert_eq!(scoped["totals"]["requests"], 1, "{scoped}");
        assert_eq!(
            scoped["totals"]["reported_requests"],
            i32::from(counts.is_some()),
            "{scoped}"
        );
        let bills = report(
            &emp,
            &format!("/api/usage?sessions=true&start=0&end=4102444800&{filter}"),
        );
        assert_eq!(bills["total"], 1, "{bills}");
        assert_eq!(bills["records"][0]["session_id"], *session);
        assert_eq!(bills["records"][0]["requests"], 1);
        if counts.is_none() {
            assert_eq!(row["pricing"]["cost_nanos"], Value::Null);
            assert_eq!(row["pricing"]["price_issue"], "missing_usage");
            assert_eq!(scoped["totals"]["priced_requests"], 0);
            assert_eq!(scoped["totals"]["unpriced_requests"], 1);
            assert_eq!(bills["records"][0]["reported_requests"], 0);
        }
    }
    let before = billing_snapshot(&emp);
    assert_eq!(before["totals"]["requests"], 4);
    assert_eq!(before["totals"]["reported_requests"], 3);
    assert_eq!(before["totals"]["priced_requests"], 3);
    assert_eq!(before["totals"]["input_tokens"], 25);
    assert_eq!(before["totals"]["output_tokens"], 11);
    assert_eq!(before["totals"]["cost_nanos"], 138_000);
    emp.stop();
    let restarted = workspace.start_without_path_cli();
    assert_eq!(billing_snapshot(&restarted), before);
    for upstream in [&subscriptions, &known_api, &unknown_api] {
        upstream.assert_idle();
    }
}

#[test]
fn repeated_upstream_response_usage_is_billed_once() {
    let upstream = Upstream::start();
    upstream.reply_json(&json!({"id":"resp_replayed_usage", "object":"response",
        "status":"completed", "model":"billing-model", "output":[],
        "usage":{"input_tokens":7, "output_tokens":3, "total_tokens":10}}));
    let workspace = Workspace::new();
    workspace.write_config(json!({"providers":[{"id":"billing-api", "base_url":upstream.base_url(),
        "protocol":"responses", "auth_mode":"api_key", "api_key":"billing-api-key"}],
        "models":[{"id":"billing-api/model", "provider":"billing-api", "upstream_id":"billing-model", "enabled":true}]}));
    seed_prices(&workspace);
    let emp = workspace.start_without_path_cli();
    let mut requests = Vec::new();
    for _ in 0..2 {
        let response = emp.responses(
            &json!({"model":"billing-api/model", "input":"replay fixture",
            "metadata":{"session_id":"replay-session"}}),
        );
        assert_eq!(response.status, 200, "{}", response.text());
        requests.push(upstream.next_forwarded().headers["x-emp-request-id"].clone());
    }
    assert_ne!(requests[0], requests[1]);
    let before = billing_snapshot(&emp);
    assert_eq!(before["call_count"], 2);
    assert_eq!(before["totals"]["requests"], 1);
    assert_eq!(before["totals"]["input_tokens"], 7);
    assert_eq!(before["totals"]["output_tokens"], 3);
    assert_eq!(before["totals"]["cost_nanos"], 38_000);
    assert_eq!(before["bills"][0]["session_id"], "replay-session");
    assert_eq!(before["bills"][0]["requests"], 1);
    emp.stop();
    let restarted = workspace.start_without_path_cli();
    assert_eq!(billing_snapshot(&restarted), before);
    upstream.assert_idle();
}
