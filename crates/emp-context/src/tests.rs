use super::*;
use serde_json::json;
use std::collections::BTreeMap;

struct Reader(Vec<VisibleItem>);

impl HistoryReader for Reader {
    fn read_compaction_history(
        &self,
        anchor: &HistoryAnchor,
        _: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError> {
        Ok(HistorySnapshot {
            thread_id: anchor.thread_id.clone().expect("thread"),
            items: self.0.clone(),
            source_model: Some("gpt-native".to_owned()),
        })
    }
}

fn assert_owned_matches_borrowed(
    body: &Value,
    native_destination: bool,
    reader: &Reader,
    incoming: &BTreeMap<String, String>,
) {
    let expected = prepare(body, incoming, native_destination, reader);
    let actual = prepare_owned(body.clone(), incoming, native_destination, reader);
    assert_eq!(actual, expected);
}

fn opaque_compaction_fixture() -> (Value, Reader) {
    let mut call = VisibleItem::new("tool_call", json!({"name":"read_file","arguments":"{}"}));
    call.call_id = Some("call-1".to_owned());
    call.raw_type = Some("custom_tool_call".to_owned());
    let reader = Reader(vec![
        VisibleItem::new("user_message", json!("old requirement")),
        call,
        VisibleItem::new("compaction_marker", json!("")),
        VisibleItem::new("assistant_message", json!("duplicate active tail")),
    ]);
    let body = json!({
        "model":"external/model",
        "input":[
            {"type":"compaction","encrypted_content":"opaque"},
            {"type":"message","role":"user","content":"active tail"}
        ],
        "client_metadata":{"x-codex-turn-metadata":"{\"thread_id\":\"thread\",\"turn_id\":\"turn\"}"}
    });
    (body, reader)
}

#[test]
fn owned_prepare_preserves_large_ordinary_body_allocations() {
    for native_destination in [false, true] {
        let body = json!({"input":[{
            "type":"message","role":"user","content":[{
                "type":"input_text","text":"x".repeat(1024 * 1024)
            }]
        }]});
        let original_ptr = body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .as_ptr();
        let prepared = prepare_owned(
            body,
            &BTreeMap::new(),
            native_destination,
            &Reader(Vec::new()),
        )
        .expect("ordinary input preparation");
        let prepared_ptr = prepared["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .as_ptr();
        assert_eq!(prepared_ptr, original_ptr);
    }
}

#[test]
fn previous_response_fast_path_precedes_compaction_decoding() {
    let body = json!({
        "previous_response_id":"resp_previous",
        "input":[{"type":"compaction","encrypted_content":"emp1:invalid"}]
    });
    let original_ptr = body["input"][0]["encrypted_content"]
        .as_str()
        .unwrap()
        .as_ptr();
    let prepared = prepare_owned(body, &BTreeMap::new(), false, &Reader(Vec::new()))
        .expect("previous response body is passed through");
    assert_eq!(
        prepared["input"][0]["encrypted_content"]
            .as_str()
            .unwrap()
            .as_ptr(),
        original_ptr
    );
}

#[test]
fn owned_prepare_matches_borrowed_prepare_for_external_opaque_compaction() {
    let (body, reader) = opaque_compaction_fixture();
    let incoming = BTreeMap::new();
    assert_owned_matches_borrowed(&body, false, &reader, &incoming);
}

#[test]
fn owned_prepare_matches_borrowed_prepare_for_portable_compaction_destinations() {
    let body = json!({
        "input":[{"type":"compaction","encrypted_content":"emp1:U3VtbWFyeSB0ZXh0Lg=="}]
    });
    let reader = Reader(Vec::new());
    let incoming = BTreeMap::new();
    assert_owned_matches_borrowed(&body, false, &reader, &incoming);
    assert_owned_matches_borrowed(&body, true, &reader, &incoming);
}

#[test]
fn native_opaque_and_invalid_portable_compactions_match_borrowed_prepare() {
    let (opaque, reader) = opaque_compaction_fixture();
    assert_owned_matches_borrowed(&opaque, true, &reader, &BTreeMap::new());

    let invalid = json!({
        "input":[{"type":"compaction","encrypted_content":"emp1:invalid"}]
    });
    assert_owned_matches_borrowed(&invalid, true, &Reader(Vec::new()), &BTreeMap::new());
}

#[test]
fn portable_compaction_decodes_without_a_history_reader() {
    let body =
        json!({"input":[{"type":"compaction","encrypted_content":"emp1:U3VtbWFyeSB0ZXh0Lg=="}]});
    let projected = prepare(&body, &BTreeMap::new(), false, &Reader(Vec::new())).unwrap();
    assert!(projected.to_string().contains("Summary text."));
    assert!(!projected.to_string().contains("emp1:"));
}

#[test]
fn opaque_checkpoint_replaces_only_the_hidden_prefix_and_keeps_tool_pairs() {
    let mut call = VisibleItem::new("tool_call", json!({"name":"read_file","arguments":"{}"}));
    call.call_id = Some("call-1".to_owned());
    call.raw_type = Some("custom_tool_call".to_owned());
    let reader = Reader(vec![
        VisibleItem::new("user_message", json!("old requirement")),
        call,
        VisibleItem::new("compaction_marker", json!("")),
        VisibleItem::new("assistant_message", json!("duplicate active tail")),
    ]);
    let body = json!({
        "model":"external/model",
        "input":[
            {"type":"compaction","encrypted_content":"opaque"},
            {"type":"message","role":"user","content":"active tail"}
        ],
        "client_metadata":{"x-codex-turn-metadata":"{\"thread_id\":\"thread\",\"turn_id\":\"turn\"}"}
    });
    let projected = prepare(&body, &BTreeMap::new(), false, &reader).unwrap();
    let input = projected["input"].as_array().unwrap();
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
    assert_eq!(input.last().unwrap()["content"], "active tail");
}
