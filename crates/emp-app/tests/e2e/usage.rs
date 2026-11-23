use crate::support::{Workspace, write_file};
use serde_json::{Value, json};

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
