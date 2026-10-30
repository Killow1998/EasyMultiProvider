use super::*;
use serde_json::json;

fn route(context_window: u64) -> ResolvedRoute {
    emp_core::resolved_route_from_parts(
        "claude/model",
        json!({
            "id":"claude", "protocol":"anthropic_messages", "auth_mode":"api_key",
            "base_url":"https://example.invalid/v1", "execution_backend":"claude_cli"
        })
        .as_object()
        .unwrap()
        .clone(),
        json!({
            "id":"claude/model", "provider":"claude", "upstream_id":"sonnet",
            "context_window":context_window, "output_limit":128_000,
            "capability_sources":{"context_window":{"source":"advertised","confidence":0.75}}
        })
        .as_object()
        .unwrap()
        .clone(),
        emp_core::RouteSource::ExplicitModel,
    )
    .unwrap()
}

#[test]
fn oversized_claude_history_can_reach_compaction_before_cli_input_validation() {
    let candidate = route(1_000_000);
    let mut input = (0..48)
        .map(|index| {
            json!({
                "type":"message", "role":"user",
                "content":format!("visible history {index}: {}", "h".repeat(256 * 1024))
            })
        })
        .collect::<Vec<_>>();
    let active = vec![
        json!({"type":"message","role":"user","content":"active request"}),
        json!({"type":"function_call","call_id":"active-call","name":"read","arguments":"{}"}),
        json!({"type":"function_call_output","call_id":"active-call","output":"active result"}),
    ];
    let active_start = input.len();
    input.extend(active.iter().cloned());
    let body = json!({
        "model":"claude/model", "instructions":"keep this instruction",
        "input":input, "max_output_tokens":4096,
        (emp_history::ACTIVE_INPUT_START):active_start,
        "tools":[{"type":"function","name":"read","parameters":{"type":"object"}}]
    });
    let assessment = assess_destination_context(&candidate, &body)
        .unwrap_or_else(|_| panic!("oversized history must be assessed before it is sent to CLI"));
    assert!(assessment.blocked());
    assert!(assessment.input_estimate.unwrap() > assessment.safe_input_limit.unwrap());
    assert_eq!(assessment.output_reserve, Some(4096));
    assert_eq!(assessment.safe_input_limit, Some(995648));

    let mut summaries = 0;
    let compacted = emp_history::context::compact_with(
        &body,
        candidate.model.value(),
        assessment.safe_input_limit.unwrap(),
        |summary| {
            crate::services::claude_cli::context_estimation_payload(summary)
                .expect("summary fits the CLI adapter budget");
            summaries += 1;
            Ok("visible portable checkpoint".into())
        },
    )
    .unwrap();
    assert!((1..=16).contains(&summaries));
    let final_assessment = assess_destination_context(&candidate, &compacted)
        .unwrap_or_else(|_| panic!("compacted history fits the CLI adapter budget"));
    assert!(!final_assessment.blocked());
    let final_input = compacted["input"].as_array().unwrap();
    assert_eq!(&final_input[final_input.len() - active.len()..], &active);
    assert_eq!(compacted["instructions"], body["instructions"]);
    assert_eq!(compacted["tools"], body["tools"]);
    assert!(
        final_input[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("visible portable checkpoint")
    );
    assert_eq!(body["input"].as_array().unwrap().len(), 51);
    assert_eq!(body[emp_history::ACTIVE_INPUT_START], active_start);
}

#[test]
fn oversized_active_input_and_fixed_instructions_are_not_silently_discarded() {
    let candidate = route(1_000_000);
    for fixed_instructions in [false, true] {
        let large = "must preserve ".repeat(900_000);
        let body = if fixed_instructions {
            json!({"instructions":large,"input":"active request"})
        } else {
            json!({"input":[{"role":"user","content":large}]})
        };
        let assessment = assess_destination_context(&candidate, &body)
            .unwrap_or_else(|_| panic!("bounded estimate must reach history assessment"));
        assert!(assessment.blocked());
        let result = emp_history::context::compact_with(
            &body,
            candidate.model.value(),
            assessment.safe_input_limit.unwrap(),
            |_| panic!("fixed instructions and the active turn cannot be summarized away"),
        );
        assert_eq!(result, Err("compaction_unit_too_large"));
    }
}

#[test]
fn invalid_claude_media_is_not_reclassified_as_an_unavailable_size_estimate() {
    let body = json!({"input":[{"role":"user","content":[
        {"type":"input_audio","input_audio":{"data":"AAAA"}}
    ]}]});
    assert!(matches!(
        assess_destination_context(&route(1_000_000), &body),
        Err(DestinationPrepareError::ClaudeCli(
            crate::services::claude_cli::ClaudeCliError::Failure("unsupported_input_modality")
        ))
    ));
}
