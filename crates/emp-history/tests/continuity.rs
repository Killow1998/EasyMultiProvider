//! History continuity: users resuming Codex threads through EMP must get
//! their visible history back with stable request identity.

use emp_history::{
    HistoryError, HistoryReader, HistorySnapshot, VisibleItem, prepare, request_history_anchor,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

struct Reader(Vec<VisibleItem>);

impl HistoryReader for Reader {
    fn read_compaction_history(
        &self,
        anchor: &emp_history::HistoryAnchor,
        _: &serde_json::Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError> {
        Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().expect("thread"),
            items: self.0.clone(),
            source_model: Some("gpt-native".to_owned()),
        })
    }
}

fn call() -> VisibleItem {
    let mut call = VisibleItem::new("tool_call", json!({"name": "read", "arguments": "{}"}));
    call.call_id = Some("call-1".to_owned());
    call.raw_type = Some("custom_tool_call".to_owned());
    call
}

#[test]
fn anchors_take_headers_first_and_refuse_conflicting_identity() {
    let metadata = r#"{"thread_id":"thread","turn_id":"turn","window_id":"new"}"#;
    // Header thread wins over metadata; window falls back to metadata.
    let anchor = request_history_anchor(
        json!({}).as_object().unwrap(),
        &headers(&[("thread-id", "thread"), ("x-codex-turn-metadata", metadata)]),
    )
    .expect("anchor");
    assert_eq!(anchor.thread_id.as_deref(), Some("thread"));
    assert_eq!(anchor.window_id.as_deref(), Some("new"));

    // A stale window header conflicting with fresh metadata fails closed.
    let error = request_history_anchor(
        json!({}).as_object().unwrap(),
        &headers(&[
            ("x-codex-turn-metadata", metadata),
            ("x-codex-window-id", "stale"),
        ]),
    )
    .expect_err("window conflict");
    assert_eq!(error.reason(), "conflicting_window_identity");

    // Conflicting thread identity cannot silently fork the history.
    let error = request_history_anchor(
        json!({}).as_object().unwrap(),
        &headers(&[
            ("thread-id", "different"),
            ("x-codex-turn-metadata", metadata),
        ]),
    )
    .expect_err("thread conflict");
    assert_eq!(error.reason(), "conflicting_thread_identity");

    // Malformed metadata is rejected instead of silently ignored.
    let error = request_history_anchor(
        json!({}).as_object().unwrap(),
        &headers(&[("x-codex-turn-metadata", "not json")]),
    )
    .expect_err("malformed metadata");
    assert_eq!(error.reason(), "invalid_turn_metadata");
}

#[test]
fn opaque_compaction_hides_only_the_prefix_and_keeps_pairs_intact() {
    let reader = Reader(vec![
        VisibleItem::new("user_message", json!("old requirement")),
        call(),
        VisibleItem::new("compaction_marker", json!("")),
        VisibleItem::new("assistant_message", json!("duplicate active tail")),
    ]);
    let body = json!({
        "model": "external/model",
        "input": [
            {"type": "compaction", "encrypted_content": "opaque"},
            {"type": "message", "role": "user", "content": "active tail"}
        ],
        "client_metadata": {"x-codex-turn-metadata":
            "{\"thread_id\":\"thread\",\"turn_id\":\"turn\"}"}
    });
    let projected = prepare(&body, &BTreeMap::new(), false, &reader).expect("prepared");
    let input = projected["input"].as_array().expect("input");
    assert!(!projected.to_string().contains("opaque"));
    assert!(!projected.to_string().contains("duplicate active tail"));
    assert!(projected.to_string().contains("old requirement"));
    assert_eq!(
        input
            .iter()
            .filter(|item| item.get("call_id") == Some(&json!("call-1")))
            .count(),
        2
    );
    assert_eq!(input.last().expect("tail")["content"], "active tail");
    // The boundary tells providers where replayed history ends.
    assert_eq!(projected[emp_history::ACTIVE_INPUT_START], 3);
}

#[test]
fn portable_compaction_decodes_without_a_reader_and_native_passes_opaque() {
    // Portable summaries are self-contained: no SQLite access needed.
    let body = json!({"input": [
        {"type": "compaction", "encrypted_content": "emp1:U3VtbWFyeSB0ZXh0Lg=="}
    ]});
    let projected =
        prepare(&body, &BTreeMap::new(), false, &Reader(Vec::new())).expect("portable decode");
    assert!(projected.to_string().contains("Summary text."));

    // Native destinations keep the opaque item for the provider itself.
    let opaque = json!({"input": [
        {"type": "compaction", "encrypted_content": "opaque"}
    ]});
    let projected =
        prepare(&opaque, &BTreeMap::new(), true, &Reader(Vec::new())).expect("native pass");
    assert_eq!(projected["input"][0]["encrypted_content"], "opaque");

    // Corrupt portable summaries fail instead of replaying garbage.
    let corrupt = json!({"input": [
        {"type": "compaction", "encrypted_content": "emp1:!!!not base64!!!"}
    ]});
    let error = prepare(&corrupt, &BTreeMap::new(), true, &Reader(Vec::new()))
        .expect_err("corrupt portable summary");
    assert_eq!(error.reason(), "portable_checkpoint_invalid");
}

#[test]
fn reconstruction_requires_thread_and_turn_identity() {
    let reader = Reader(vec![VisibleItem::new("user_message", json!("old"))]);
    let body = json!({"input": [
        {"type": "compaction", "encrypted_content": "opaque"}
    ]});
    let error = prepare(&body, &BTreeMap::new(), false, &reader).expect_err("no identity headers");
    assert_eq!(error.reason(), "thread_identity_missing");

    // Turn identity alone is not enough either.
    let error = prepare(&body, &headers(&[("thread-id", "thread")]), false, &reader)
        .expect_err("missing turn");
    assert_eq!(error.reason(), "turn_identity_missing");

    // A reader that returns another thread's history is refused.
    struct WrongThread;
    impl HistoryReader for WrongThread {
        fn read_compaction_history(
            &self,
            _: &emp_history::HistoryAnchor,
            _: &serde_json::Map<String, Value>,
        ) -> Result<HistorySnapshot, HistoryError> {
            Ok(HistorySnapshot {
                thread_id: "other-thread".to_owned(),
                items: Vec::new(),
                source_model: None,
            })
        }
    }
    let error = prepare(
        &body,
        &headers(&[
            ("thread-id", "thread"),
            ("x-codex-turn-metadata", r#"{"turn_id":"turn"}"#),
        ]),
        false,
        &WrongThread,
    )
    .expect_err("cross-thread snapshot");
    assert_eq!(error.reason(), "thread_mismatch");
}
