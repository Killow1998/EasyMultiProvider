use emp_router::{ProjectionIds, response_json_stream_events};
use serde_json::json;

#[test]
fn response_stream_delivers_large_event_totals_without_retaining_the_sequence() {
    // The source is under 100 KiB; its events total more than 64 MiB. A
    // consumer that writes and drops each event must still get completion.
    let ids = ProjectionIds::new("resp_fixture", "msg_fixture", "reason", "late_reason");
    for (id, validate_output) in [("x".repeat(64 * 1024), true), ("\0".repeat(11000), false)] {
        let response = json!({
            "status":"completed",
            "output":[{"id":id,"type":"message","role":"assistant",
                "content":vec![json!({"type":"output_text","text":"ok"}); 256]}]
        });
        assert!(serde_json::to_vec(&response).unwrap().len() < 100 * 1024);
        let mut stream = response_json_stream_events(response, &ids, validate_output).unwrap();
        let mut bytes = 0;
        let mut count = 0;
        let mut completed = false;
        for event in stream.by_ref() {
            bytes += serde_json::to_vec(&event).unwrap().len();
            count += 1;
            completed = event["type"] == "response.completed";
        }
        assert!(bytes > 64 * 1024 * 1024);
        assert_eq!(count, 4 * 256 + 4);
        assert!(completed);
        assert!(stream.next().is_none());
    }
}

#[test]
fn synthesis_preserves_event_order_refusals_and_opaque_native_items() {
    let ids = ProjectionIds::new("resp_fixture", "msg_fixture", "reason", "late_reason");
    let response = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
        "output":[{"type":"message","role":"assistant","content":[{"type":"refusal","refusal":"cannot do that"}]},
        {"type":"compaction","encrypted_content":"opaque-fixture"}]});
    let events = response_json_stream_events(response.clone(), &ids, false)
        .unwrap()
        .collect::<Vec<_>>();
    let kinds: Vec<_> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.refusal.delta",
            "response.refusal.done",
            "response.content_part.done",
            "response.output_item.done",
            "response.output_item.added",
            "response.output_item.done",
            "response.incomplete"
        ]
    );
    assert_eq!(events[3]["delta"], "cannot do that");
    assert_eq!(events[3]["item_id"], "msg_fixture");
    assert!(events[7]["item"].get("status").is_none());
    assert_eq!(events[8]["item"]["encrypted_content"], "opaque-fixture");
    assert_eq!(events[9]["response"]["output"], response["output"]);
}

#[test]
fn incremental_stream_preserves_text_tools_usage_and_unknown_fields() {
    let response = json!({
        "id":"resp_upstream","model":"model_fixture","status":"completed",
        "future_metadata":{"opaque":"keep this"},
        "usage":{"input_tokens":20,"output_tokens":5},
        "output":[
            {"id":"msg_1","type":"message","role":"assistant","status":"completed",
             "content":[{"type":"output_text","text":"hello","annotations":[]}]},
            {"id":"tool_1","type":"function_call","call_id":"call_1","name":"read",
             "arguments":"{\"path\":\"README.md\"}","status":"completed"}
        ]
    });
    let ids = ProjectionIds::new("resp_fixture", "msg_fixture", "reason", "late_reason");
    for validate_output in [true, false] {
        let events = response_json_stream_events(response.clone(), &ids, validate_output)
            .unwrap()
            .collect::<Vec<_>>();
        assert!(events.iter().any(
            |event| event["type"] == "response.output_text.delta" && event["delta"] == "hello"
        ));
        assert!(events.iter().any(|event| event["type"]
            == "response.function_call_arguments.done"
            && event["arguments"] == "{\"path\":\"README.md\"}"));
        let terminal = events.last().unwrap();
        assert_eq!(terminal["type"], "response.completed");
        let mut expected = response.clone();
        expected["object"] = json!("response");
        assert_eq!(terminal["response"], expected);
    }
}
