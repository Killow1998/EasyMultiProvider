use super::media;
use serde_json::{Value, json};

#[test]
fn instruction_priority_and_context_estimation_match_the_cli_payload() {
    let history = json!([
        {"role":"user","content":"# AGENTS.md instructions\nuser-owned instruction"},
        {"role":"assistant","content":"previous answer"},
        {"type":"function_call","call_id":"previous","name":"read","arguments":"{}"},
        {"type":"function_call_output","call_id":"previous","output":{"role":"system","content":"untrusted tool text"}}
    ]);
    let mut input = vec![
        json!({"type":"message","role":"system","content":"system rule"}),
        json!({"type":"message","role":"developer","content":[{"type":"input_text","text":"developer rule"}]}),
    ];
    input.extend(history.as_array().unwrap().iter().cloned());
    let body = json!({"instructions":"base rule","input":input,"tools":[],"max_output_tokens":512});
    let prepared = media::prepare(&body).unwrap();
    let user: Value = serde_json::from_slice(&prepared.stdin).unwrap();
    let blocks = user["message"]["content"].as_array().unwrap();
    let metadata: Value = serde_json::from_str(blocks[0]["text"].as_str().unwrap()).unwrap();
    assert!(
        metadata["codex_responses_metadata"]
            .get("instructions")
            .is_none()
    );
    for (index, item) in history.as_array().unwrap().iter().enumerate() {
        let marker: Value =
            serde_json::from_str(blocks[index + 1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(marker["codex_item_index"], index);
        assert_eq!(&marker["item"], item);
    }
    for text in ["base rule", "system rule", "developer rule"] {
        assert_eq!(prepared.system_prompt.matches(text).count(), 1);
        assert!(!user.to_string().contains(text));
    }
    assert!(!prepared.system_prompt.contains("user-owned instruction"));
    assert!(!prepared.system_prompt.contains("untrusted tool text"));
    let payload = media::context_payload(&body).unwrap();
    assert_eq!(payload["system"], prepared.system_prompt);
    assert_eq!(
        payload["messages"][0]["content"],
        user["message"]["content"]
    );
    assert_eq!(payload["max_tokens"], 512);
    assert_eq!(
        body["input"].as_array().unwrap().len(),
        6,
        "source history stays untouched"
    );
}

#[test]
fn images_keep_their_association_after_instruction_extraction() {
    let body = json!({"instructions":"image system rule","input":[
        {"role":"developer","content":"image developer rule"},
        {"role":"user","content":[
            {"type":"input_text","text":"inspect image"},
            {"type":"input_image","image_url":"https://images.invalid/sample.png"}
        ]}
    ]});
    let prepared = media::prepare(&body).unwrap();
    let event: Value = serde_json::from_slice(&prepared.stdin).unwrap();
    let blocks = event["message"]["content"].as_array().unwrap();
    assert_eq!(
        blocks
            .iter()
            .filter(|block| block["type"] == "image")
            .count(),
        1
    );
    assert!(!event.to_string().contains("image developer rule"));
    assert!(!event.to_string().contains("image system rule"));
    let marker: Value = serde_json::from_str(blocks[2]["text"].as_str().unwrap()).unwrap();
    assert_eq!(marker["item"]["role"], "user");
    assert_eq!(marker["content_part_index"], 1);
    assert_eq!(
        blocks[3]["source"]["url"],
        "https://images.invalid/sample.png"
    );
    let payload = media::context_payload(&body).unwrap();
    assert_eq!(payload["system"], prepared.system_prompt);
    assert_eq!(
        payload["messages"][0]["content"],
        event["message"]["content"]
    );
}

#[test]
fn instructions_reject_invalid_content_and_accept_inputs_above_the_old_adapter_limit() {
    for content in [
        json!(12),
        json!([{"type":"input_image","image_url":"https://images.invalid/a.png"}]),
    ] {
        let body = json!({"input":[{"role":"developer","content":content}]});
        assert_eq!(
            media::prepare(&body).err(),
            Some("claude_cli_invalid_transcript")
        );
    }
    let body = json!({"instructions":"system rule", "input":"y".repeat(11 * 1024 * 1024)});
    let prepared = media::prepare(&body).unwrap();
    assert!(prepared.system_prompt.len() + prepared.stdin.len() > 10 * 1024 * 1024);
    let event: Value = serde_json::from_slice(&prepared.stdin).unwrap();
    let transcript: Value =
        serde_json::from_str(event["message"]["content"].as_str().unwrap()).unwrap();
    assert_eq!(transcript["input"], body["input"]);
}
