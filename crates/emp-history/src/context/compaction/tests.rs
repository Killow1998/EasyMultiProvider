use super::summary::{MAP_PROMPT, MAX_SUMMARY_REQUESTS, message, summary_body, summary_item};
use super::*;
use crate::context::assess;
use serde_json::json;

#[test]
fn assessment_and_compaction_preserve_the_active_turn() {
    let provider = Map::from_iter([
        ("context_window".to_owned(), json!(1200)),
        (
            "capability_sources".to_owned(),
            json!({"context_window":{"source":"manual","confidence":1.0}}),
        ),
    ]);
    let model = Map::from_iter([("output_limit".to_owned(), json!(64))]);
    let body = json!({"model":"external/model","input":[
            message(&"x".repeat(500)), message(&"y".repeat(500)),
            message(&"z".repeat(500)), message(&"w".repeat(500)),
            message("active request")
        ],"max_output_tokens":64});
    let payload = json!({"messages":body["input"],"max_tokens":64});
    let assessment = assess(&provider, &model, "chat_completions", &payload);
    assert!(assessment.blocked());
    let compacted = compact_with(&body, &model, assessment.safe_input_limit.unwrap(), |_| {
        Ok("checkpoint".to_owned())
    })
    .unwrap();
    assert!(compacted.to_string().contains("checkpoint"));
    assert!(compacted.to_string().contains("active request"));
    assert!(!compacted.to_string().contains(&"x".repeat(500)));
}

#[test]
fn incremental_estimate_matches_materialized_request_estimates() {
    let root = Map::from_iter([
        ("instructions".to_owned(), json!("be brief ☃")),
        ("tools".to_owned(), json!([{"type":"function","name":"f"}])),
    ]);
    let items = [
        message("plain \"quoted\" text\n"),
        json!({"type":"function_call","call_id":"c","arguments":"{}"}),
        json!({"type":"input_image","image_url":{"url":"data:private","detail":"high"}}),
        json!({"type":"message","content":[{"type":"output_image","image_data":"private"}]}),
        message(&"🌎".repeat(33)),
    ];
    let active = [message("active")];
    let mut estimate = IncrementalEstimate::new(
        &input_view_value(&final_body(&root, Some("xx"), &[], &active, &[])),
        2,
    );
    let mut summary = IncrementalEstimate::new(&summary_body("m", &[], MAP_PROMPT, 7), 1);
    let mut retained = Vec::new();
    for item in items {
        let cost = [json_cost(&item, 2).unwrap()];
        let summary_cost = [json_cost(&summary_item(item.clone()), 2).unwrap()];
        retained.insert(0, vec![item]);
        let direct = estimate_json_tokens(&input_view_value(&final_body(
            &root,
            Some("xx"),
            &retained,
            &active,
            &[],
        )));
        assert_eq!(estimate.tokens_with(&cost), direct);
        assert_eq!(
            summary.tokens_with(&summary_cost),
            estimate_json_tokens(&summary_body("m", &retained, MAP_PROMPT, 7))
        );
        estimate.extend(&cost);
        summary.extend(&summary_cost);
    }
}

#[test]
fn compaction_of_many_messages_preserves_active_turn_and_bounds_calls() {
    let mut input = (0..20_000)
        .map(|index| message(&format!("small message {index}")))
        .collect::<Vec<_>>();
    input.push(message("active request"));
    let body = json!({"model":"external/model","max_output_tokens":64,"input":input});
    let mut calls = 0;
    let compacted = compact_with(&body, &Map::new(), 400_000, |_| {
        calls += 1;
        Ok("checkpoint".to_owned())
    })
    .unwrap();
    assert!((1..=4).contains(&calls));
    assert!(compacted.to_string().contains("active request"));
}

#[test]
fn compaction_keeps_recent_tool_pairs_and_preserves_summary_shape() {
    let body = json!({
        "model":"m","max_output_tokens":64,"instructions":"keep these instructions",
        "tools":[{"type":"function","name":"lookup"}],
        "input":[
            {"type":"message","role":"user","turn_id":"old-1",
                "content":[{"type":"input_text","text":"old ".repeat(330)}]},
            {"type":"message","role":"user","turn_id":"old-2",
                "content":[{"type":"input_text","text":"old ".repeat(330)}]},
            {"type":"message","role":"user","turn_id":"old-3",
                "content":[{"type":"input_text","text":"old ".repeat(330)}]},
            {"type":"message","role":"user","turn_id":"old-4",
                "content":[{"type":"input_text","text":"old ".repeat(330)}]},
            {"type":"function_call","call_id":"c1","turn_id":"tool",
                "name":"lookup","arguments":"{\"query\":\"weather\"}"},
            {"type":"function_call_output","call_id":"c1","turn_id":"tool",
                "output":"r".repeat(180)},
            {"type":"message","role":"user","turn_id":"active",
                "content":[{"type":"input_text","text":"active request"}]},
            {"type":"compaction_trigger"}
        ],
        "_emp_active_input_start":6
    });
    let mut requests = Vec::new();
    let result = compact_with(&body, &Map::new(), 1_200, |request| {
        requests.push(request.clone());
        Ok(format!("summary {}", requests.len()))
    })
    .expect("compaction succeeds");

    let input = result["input"].as_array().expect("compacted input");
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|request| {
        request["stream"] == false
            && request["tools"] == json!([])
            && request["max_output_tokens"].as_u64().unwrap() > 0
            && !request.to_string().contains("c1")
    }));
    assert!(
        requests
            .iter()
            .any(|request| request.to_string().contains("old "))
    );
    assert!(result.to_string().contains("keep these instructions"));
    assert!(result.to_string().contains("summary "));
    assert!(result.to_string().contains("active request"));
    assert_eq!(
        input.iter().filter(|item| item["call_id"] == "c1").count(),
        2
    );
    assert_eq!(
        input
            .iter()
            .filter(|item| item["turn_id"]
                .as_str()
                .is_some_and(|turn| turn.starts_with("old-")))
            .count(),
        1
    );
    assert_eq!(input.last().unwrap()["type"], "compaction_trigger");
}

#[test]
fn compaction_keeps_summary_and_unit_failure_reasons() {
    let history = (0..20)
        .map(|index| message(&format!("history {index} {}", "x".repeat(300))))
        .collect::<Vec<_>>();
    let mut input = history;
    input.push(message("active request"));
    let body = json!({
        "model":"m","max_output_tokens":64,
        "input":input,
        "_emp_active_input_start":20
    });
    assert_eq!(
        compact_with(&body, &Map::new(), 1_000, |_| Err(())),
        Err("summary_call_failed")
    );

    let oversized = json!({
        "model":"m","max_output_tokens":64,
        "input":[message(&"large ".repeat(2_000)), message("active request")],
        "_emp_active_input_start":1
    });
    let mut calls = 0;
    assert_eq!(
        compact_with(&oversized, &Map::new(), 1_000, |_| {
            calls += 1;
            Ok("checkpoint".to_owned())
        }),
        Err("compaction_unit_too_large")
    );
    assert_eq!(calls, 0);
}

#[test]
fn invalid_budget_input_and_oversized_summary_have_precise_reasons() {
    assert_eq!(
        compact_with(&json!({}), &Map::new(), 0, |_| unreachable!()),
        Err("compaction_budget_invalid")
    );
    assert_eq!(
        compact_with(&Value::Null, &Map::new(), 1000, |_| unreachable!()),
        Err("invalid_history_projection")
    );
    // Several small units exceed the final budget while each map request still
    // fits with the summary prompt. A single oversized unit fails earlier.
    let mut input = vec![message(&"x".repeat(500)); 5];
    input.push(message("active"));
    let body = json!({"model":"m", "max_output_tokens":64,
        "input":input, "_emp_active_input_start":5});
    let budget = estimate_json_tokens(&input_view_value(&body)).unwrap() - 1;
    let mut calls = 0;
    let error = compact_with(&body, &Map::new(), budget, |_| {
        calls += 1;
        Ok("summary ".repeat(1000))
    })
    .unwrap_err();
    assert_eq!(error, "compaction_result_over_budget");
    assert_eq!(calls, 1);
}

#[test]
fn compaction_rejects_unbounded_work() {
    let mut input = (0..MAX_COMPACTION_UNITS + 1)
        .map(|_| message("u"))
        .collect::<Vec<_>>();
    input.push(message("active request"));
    let body = json!({"model":"external/model","input":input});
    assert_eq!(
        compact_with(&body, &Map::new(), 4096, |_| Ok("checkpoint".to_owned())),
        Err("compaction_unit_too_large")
    );
    let mut input = (0..2_000)
        .map(|index| message(&format!("{index} {}", "m".repeat(200))))
        .collect::<Vec<_>>();
    input.push(message("active request"));
    let body = json!({"model":"external/model","max_output_tokens":64,"input":input});
    let mut calls = 0;
    assert_eq!(
        compact_with(&body, &Map::new(), 600, |_| {
            calls += 1;
            Ok("checkpoint".to_owned())
        }),
        Err("compaction_unit_too_large")
    );
    assert!(calls <= MAX_SUMMARY_REQUESTS);
}
