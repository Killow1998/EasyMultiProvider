use super::*;
#[test]
fn recovery_requires_matching_scope_recent_rejection_and_deduplicates_bytes() {
    let prior = json!({"dialect":"codex_native","route":"responses","thread_ref":"t","session_ref":"s","turn_ref":"u","provider_id":"p","model_id":"m","has_previous_response":true,"error_code":"previous_response_not_found","request_id":"aaaaaaaaaaaaaaaa","observed_at":"2026-10-12T00:00:00Z"});
    let recent = VecDeque::from([prior.clone()]);
    let mut current = prior.clone();
    current["has_previous_response"] = json!(false);
    current["request_id"] = json!("bbbbbbbbbbbbbbbb");
    current["observed_at"] = json!("2026-10-12T00:00:05Z");
    current["error_code"] = json!("");
    current["request_bytes"] = json!(10000);
    assert_eq!(
        previous_rejection(&recent, &current),
        Some("aaaaaaaaaaaaaaaa")
    );
    for dialect in ["chat_completions", "anthropic_messages", "responses"] {
        current["dialect"] = json!(dialect);
        assert_eq!(
            previous_rejection(&recent, &current),
            Some("aaaaaaaaaaaaaaaa")
        );
    }
    for key in [
        "thread_ref",
        "session_ref",
        "turn_ref",
        "provider_id",
        "model_id",
    ] {
        let mut other = current.clone();
        other[key] = json!("different");
        assert!(previous_rejection(&recent, &other).is_none());
    }
    let mut late = current.clone();
    late["observed_at"] = json!("2026-10-12T00:01:01Z");
    assert!(previous_rejection(&recent, &late).is_none());
    current["continuation_recovery_from"] = prior["request_id"].clone();
    let stats = summary(&[current.clone(), current]);
    assert_eq!(stats["count"], 1);
    assert_eq!(stats["request_json_bytes"], 10000);
}
