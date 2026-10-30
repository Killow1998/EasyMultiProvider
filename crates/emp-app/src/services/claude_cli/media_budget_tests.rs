use super::*;

#[test]
fn appending_history_preserves_existing_cache_blocks_and_tool_pairs() {
    let mut body = json!({"model":"m","tools":[],"input":[
        {"role":"user","content":[{"type":"input_text","text":"reference".repeat(2048)}]},
        {"type":"function_call","call_id":"read-1","name":"read","arguments":"{\"path\":\"a\"}"},
        {"type":"function_call_output","call_id":"read-1","output":[{"type":"input_text","text":"opaque result"}]}
    ]});
    let original = body["input"].as_array().unwrap().clone();
    let first = prepare(&body).unwrap();
    body["input"].as_array_mut().unwrap().extend([
        json!({"role":"assistant","content":"previous answer"}),
        json!({"role":"user","content":"continue"}),
    ]);
    let later = prepare(&body).unwrap();
    let ExpectedUserContent::Blocks(first_blocks) = first.expected_user_content else {
        panic!("history must expose reusable content-block boundaries");
    };
    let ExpectedUserContent::Blocks(later_blocks) = later.expected_user_content else {
        panic!("appended history must remain structured");
    };
    assert_eq!(&later_blocks[..first_blocks.len()], first_blocks);
    for (index, item) in original.iter().enumerate() {
        let marker: Value =
            serde_json::from_str(first_blocks[index + 1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(marker["codex_item_index"], index);
        assert_eq!(&marker["item"], item);
    }
    assert_eq!(
        context_payload(&body).unwrap()["messages"][0]["content"],
        json!(later_blocks)
    );
}

#[test]
fn text_with_many_parts_keeps_one_complete_item_without_repeating_metadata() {
    let body = json!({"input":[{
        "type":"message","role":"user","id":"x".repeat(64 * 1024),
        "content":vec![json!({"type":"input_text","text":"hello"}); 192]
    }]});
    let prepared = prepare(&body).unwrap();
    assert!(matches!(prepared.input_format, InputFormat::StreamJson));
    let ExpectedUserContent::Blocks(blocks) = prepared.expected_user_content else {
        panic!("structured history");
    };
    assert_eq!(blocks.len(), 2);
    let marker: Value = serde_json::from_str(blocks[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(marker["item"], body["input"][0]);
    assert!(prepared.stdin.len() < 100 * 1024);
}

#[test]
fn media_and_tool_output_validate_the_tail_after_large_metadata() {
    for tool_output in [false, true] {
        let mut parts = vec![json!({"type":"input_text","text":"hello"}); 192];
        parts.push(json!({"type":"input_audio","input_audio":{"data":"AAAA"}}));
        let item = if tool_output {
            json!({"type":"function_call_output","call_id":"quoted\"".repeat(8192),"output":parts})
        } else {
            json!({"type":"message","role":"user","id":"x".repeat(64 * 1024),"content":parts})
        };
        let body = json!({"input":[item]});
        assert!(serde_json::to_vec(&body).unwrap().len() < 100 * 1024);
        let error = if tool_output {
            context_payload(&body).err()
        } else {
            prepare(&body).err()
        };
        assert_eq!(error, Some("unsupported_input_modality"));
    }
}
