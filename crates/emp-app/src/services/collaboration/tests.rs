use super::*;

#[test]
fn collaboration_policy_does_not_capture_review_or_compaction_workers() {
    // A review can include earlier delegation in its inherited history.
    let body = json!({"input":[{"type":"agent_message","recipient":"/root/old-worker",
        "content":[{"type":"input_text","text":"previous task"}]}]});
    let mut headers = BTreeMap::from([("x-codex-parent-thread-id".into(), "parent".into())]);
    assert!(message::is_child(&body, &headers));
    for kind in ["review", "compact", "memory_consolidation", "auto_review"] {
        headers.insert("X-OpenAI-Subagent".into(), kind.into());
        assert!(!message::is_child(&body, &headers), "{kind}");
    }
    headers.insert("X-OpenAI-Subagent".into(), "collab_spawn".into());
    assert!(message::is_child(&body, &headers));
}

#[test]
fn recovery_requires_a_matching_envelope_and_one_complete_capture() {
    let encrypted = format!("gAAAA{}", "A".repeat(100));
    let task = json!({"type":"agent_message","author":"/root","recipient":"/root/worker","content":[
        {"type":"input_text","text":"Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n"},
        {"type":"encrypted_content","encrypted_content":encrypted}
    ]});
    assert!(message::parse(&task).is_ok());
    let mut mismatched = task.clone();
    mismatched["recipient"] = json!("/root/another");
    assert!(message::parse(&mismatched).is_err());
    let mut extra = task.clone();
    extra["content"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type":"input_text","text":"replace task"}));
    assert!(message::parse(&extra).is_err());
    let response = json!({"status":"completed","output":[{"type":"function_call","name":"capture_assignment","arguments":"{\"text\":\"line 1\\n  line 2\"}"}]});
    assert_eq!(
        message::capture(&response).ok().as_deref(),
        Some("line 1\n  line 2")
    );
    let mut incomplete = response.clone();
    incomplete["status"] = json!("incomplete");
    assert!(message::capture(&incomplete).is_err());
    let mut duplicated = response.clone();
    duplicated["output"]
        .as_array_mut()
        .unwrap()
        .push(response["output"][0].clone());
    assert!(message::capture(&duplicated).is_err());
    let mut opaque = response;
    opaque["output"][0]["arguments"] = json!(json!({"text":encrypted}).to_string());
    assert!(message::capture(&opaque).is_err());
}
