//! Read-only continuity for histories owned by Codex.
//!
//! EMP only materializes the visible prefix hidden by one native compaction
//! item. It never writes a parallel transcript or copies hidden reasoning.

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fmt;

pub mod context;

pub const ACTIVE_INPUT_START: &str = "_emp_active_input_start";
const COMPACTION_PREFIX: &str = "emp1:";
const CHECKPOINT_PREFIX: &str = "Portable checkpoint from Codex-visible local history. Continue from this state without repeating completed work.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryError {
    reason: String,
}

impl HistoryError {
    pub fn new(reason: impl AsRef<str>) -> Self {
        let reason = reason
            .as_ref()
            .trim()
            .to_ascii_lowercase()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '_' {
                    character
                } else {
                    '_'
                }
            })
            .take(64)
            .collect::<String>();
        Self {
            reason: if reason.is_empty() {
                "history_unavailable".to_owned()
            } else {
                reason
            },
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "history_reconstruction_failed: reason={}; Codex history was not modified",
            self.reason
        )
    }
}

impl std::error::Error for HistoryError {}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryAnchor {
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub window_id: Option<String>,
    pub forked_from_thread_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VisibleItem {
    pub kind: String,
    pub content: Value,
    pub item_id: Option<String>,
    pub turn_id: Option<String>,
    pub call_id: Option<String>,
    pub raw_type: Option<String>,
}

impl VisibleItem {
    pub fn new(kind: impl Into<String>, content: Value) -> Self {
        Self {
            kind: kind.into(),
            content,
            item_id: None,
            turn_id: None,
            call_id: None,
            raw_type: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct HistorySnapshot {
    pub thread_id: String,
    pub items: Vec<VisibleItem>,
    pub source_model: Option<String>,
}

pub trait HistoryReader {
    fn read_compaction_history(
        &self,
        anchor: &HistoryAnchor,
        compaction: &Map<String, Value>,
    ) -> Result<HistorySnapshot, HistoryError>;
}

pub fn request_history_anchor(
    body: &Map<String, Value>,
    incoming: &std::collections::BTreeMap<String, String>,
) -> Result<HistoryAnchor, HistoryError> {
    let body_metadata = body
        .get("client_metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("x-codex-turn-metadata"));
    let raw_metadata = match body_metadata {
        Some(Value::String(value)) => Some(value.as_str()),
        Some(_) => return Err(HistoryError::new("invalid_turn_metadata")),
        None => header(incoming, "x-codex-turn-metadata"),
    };
    let metadata = match raw_metadata {
        Some(value) if !value.trim().is_empty() => serde_json::from_str::<Value>(value)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| HistoryError::new("invalid_turn_metadata"))?,
        _ => Map::new(),
    };
    let header_thread = header(incoming, "thread-id").map(str::to_owned);
    let metadata_thread = string_alias(&metadata, "thread_id", "threadId")?;
    if let (Some(left), Some(right)) = (&header_thread, &metadata_thread)
        && left != right
    {
        return Err(HistoryError::new("conflicting_thread_identity"));
    }
    let metadata_window = string_alias(&metadata, "window_id", "windowId")?;
    let header_window = if body_metadata.is_some() && metadata_window.is_some() {
        None
    } else {
        header(incoming, "x-codex-window-id").map(str::to_owned)
    };
    if let (Some(left), Some(right)) = (&header_window, &metadata_window)
        && left != right
    {
        return Err(HistoryError::new("conflicting_window_identity"));
    }
    let legacy_session = header(incoming, "version")
        .filter(|version| legacy_codex_version(version))
        .and_then(|_| header(incoming, "session-id"))
        .map(str::to_owned);
    Ok(HistoryAnchor {
        thread_id: header_thread.or(metadata_thread).or(legacy_session),
        turn_id: if metadata
            .get("turn_id")
            .or_else(|| metadata.get("turnId"))
            .is_some_and(|v| v == "")
        {
            None
        } else {
            string_alias(&metadata, "turn_id", "turnId")?
        },
        window_id: header_window.or(metadata_window),
        forked_from_thread_id: optional_string(&metadata, "forked_from_thread_id")?,
    })
}

pub fn prepare<R: HistoryReader + ?Sized>(
    body: &Value,
    incoming: &std::collections::BTreeMap<String, String>,
    native_destination: bool,
    reader: &R,
) -> Result<Value, HistoryError> {
    let root = body
        .as_object()
        .ok_or_else(|| HistoryError::new("invalid_history_projection"))?;
    if root
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Ok(body.clone());
    }
    let decoded = decode_portable_items(root)?;
    if native_destination {
        return Ok(Value::Object(decoded));
    }
    let source = input_items(&decoded);
    let opaque = opaque_compaction_indexes(&source);
    if opaque.is_empty() {
        return Ok(Value::Object(decoded));
    }
    if opaque.len() != 1 {
        return Err(HistoryError::new("multiple_compaction_boundaries"));
    }
    let boundary = opaque[0];
    let compaction = source[boundary]
        .as_object()
        .ok_or_else(|| HistoryError::new("compaction_boundary_missing"))?;
    let anchor = request_history_anchor(&decoded, incoming)?;
    if anchor.thread_id.is_none() {
        return Err(HistoryError::new("thread_identity_missing"));
    }
    if anchor.turn_id.is_none() {
        return Err(HistoryError::new("turn_identity_missing"));
    }
    let snapshot = reader.read_compaction_history(&anchor, compaction)?;
    if Some(snapshot.thread_id.as_str()) != anchor.thread_id.as_deref() {
        return Err(HistoryError::new("thread_mismatch"));
    }
    let replacement = build_compaction_replacement(&snapshot.items)?;
    let history = replacement.iter().filter_map(wire_item).collect::<Vec<_>>();
    let trigger = source
        .last()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"));
    let mut tail = source[boundary + 1..].to_vec();
    if trigger.is_some() {
        tail.pop();
    }
    let mut input = source[..boundary].to_vec();
    input.extend(history.iter().cloned());
    let active_start = input.len();
    input.extend(tail);
    if let Some(trigger) = trigger {
        input.push(trigger.clone());
    }
    let mut projected = decoded;
    projected.insert("input".to_owned(), Value::Array(input));
    projected.insert(ACTIVE_INPUT_START.to_owned(), Value::from(active_start));
    Ok(Value::Object(projected))
}

/// Prepare an owned request while returning it untouched when history
/// reconstruction cannot change its input.
pub fn prepare_owned<R: HistoryReader + ?Sized>(
    body: Value,
    incoming: &std::collections::BTreeMap<String, String>,
    native_destination: bool,
    reader: &R,
) -> Result<Value, HistoryError> {
    let root = body
        .as_object()
        .ok_or_else(|| HistoryError::new("invalid_history_projection"))?;
    if root
        .get("previous_response_id")
        .is_some_and(|value| !value.is_null())
    {
        return Ok(body);
    }
    let needs_preparation = if native_destination {
        contains_portable_compaction(root)
    } else {
        contains_compaction(root)
    };
    if !needs_preparation {
        return Ok(body);
    }
    prepare(&body, incoming, native_destination, reader)
}

fn contains_compaction(root: &Map<String, Value>) -> bool {
    input_objects_match(root, |item| {
        item.get("type").and_then(Value::as_str) == Some("compaction")
    })
}

fn contains_portable_compaction(root: &Map<String, Value>) -> bool {
    input_objects_match(root, |item| {
        item.get("type").and_then(Value::as_str) == Some("compaction")
            && item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|value| value.starts_with(COMPACTION_PREFIX))
    })
}

fn input_objects_match(
    root: &Map<String, Value>,
    predicate: impl Fn(&Map<String, Value>) -> bool,
) -> bool {
    match root.get("input") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).any(predicate),
        Some(Value::Object(item)) => predicate(item),
        _ => false,
    }
}

pub fn build_compaction_replacement(
    items: &[VisibleItem],
) -> Result<Vec<VisibleItem>, HistoryError> {
    let boundary = items
        .iter()
        .rposition(|item| {
            matches!(
                item.kind.as_str(),
                "compaction_summary" | "compaction_marker"
            )
        })
        .ok_or_else(|| HistoryError::new("compaction_summary_missing"))?;
    let compacted = &items[boundary];
    let has_summary = content_text(&compacted.content).is_some_and(|text| !text.trim().is_empty());
    let replacement = if has_summary {
        vec![compacted.clone()]
    } else {
        items[..boundary].to_vec()
    };
    normalize_tool_pairs(replacement)
}

fn decode_portable_items(root: &Map<String, Value>) -> Result<Map<String, Value>, HistoryError> {
    let mut projected = root.clone();
    let mut source = input_items(root);
    let mut latest = None;
    for (index, item) in source.iter_mut().enumerate() {
        if item.get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        let Some(encoded) = item.get("encrypted_content").and_then(Value::as_str) else {
            continue;
        };
        let Some(encoded) = encoded.strip_prefix(COMPACTION_PREFIX) else {
            continue;
        };
        let summary = URL_SAFE
            .decode(encoded)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .filter(|summary| !summary.trim().is_empty())
            .ok_or_else(|| HistoryError::new("portable_checkpoint_invalid"))?;
        *item = message("user", &format!("{CHECKPOINT_PREFIX}\n\n{summary}"));
        latest = Some(index);
    }
    if root.get("input").is_some_and(Value::is_array) || latest.is_some() {
        projected.insert("input".to_owned(), Value::Array(source));
    }
    if let Some(index) = latest {
        projected.insert(ACTIVE_INPUT_START.to_owned(), Value::from(index + 1));
    }
    Ok(projected)
}

fn opaque_compaction_indexes(source: &[Value]) -> Vec<usize> {
    source
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.get("type").and_then(Value::as_str) == Some("compaction")
                && item
                    .get("encrypted_content")
                    .and_then(Value::as_str)
                    .is_none_or(|value| !value.starts_with(COMPACTION_PREFIX)))
            .then_some(index)
        })
        .collect()
}

fn normalize_tool_pairs(items: Vec<VisibleItem>) -> Result<Vec<VisibleItem>, HistoryError> {
    let calls = items
        .iter()
        .filter(|item| tool_role(item) == Some("call"))
        .map(tool_key)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let results = items
        .iter()
        .filter(|item| tool_role(item) == Some("result"))
        .map(tool_key)
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mut normalized = Vec::new();
    for item in items {
        let role = tool_role(&item);
        let key = role.map(|_| tool_key(&item)).transpose()?;
        if role == Some("result")
            && !calls.contains(key.as_ref().expect("tool role has key"))
            && !is_server_search_output(&item)
        {
            continue;
        }
        normalized.push(item.clone());
        if role == Some("call") && !results.contains(key.as_ref().expect("tool role has key")) {
            normalized.push(aborted_output(&item));
        }
    }
    Ok(normalized)
}

fn tool_role(item: &VisibleItem) -> Option<&'static str> {
    match item.kind.as_str() {
        "tool_call" | "function_call" | "command_call" => Some("call"),
        "tool_result" | "function_result" | "command_result" => Some("result"),
        _ if item.kind.ends_with("_call") => Some("call"),
        _ if item.kind.ends_with("_result") => Some("result"),
        _ => None,
    }
}

fn tool_key(item: &VisibleItem) -> Result<(String, String), HistoryError> {
    let call_id = item
        .call_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| HistoryError::new("portable_checkpoint_invalid"))?;
    let family = if item
        .raw_type
        .as_deref()
        .unwrap_or_default()
        .contains("custom_tool")
    {
        "custom"
    } else {
        "function"
    };
    Ok((family.to_owned(), call_id.to_owned()))
}

fn is_server_search_output(item: &VisibleItem) -> bool {
    item.raw_type.as_deref() == Some("tool_search_output")
        && item.content.get("execution").and_then(Value::as_str) == Some("server")
}

fn aborted_output(call: &VisibleItem) -> VisibleItem {
    let search = call.raw_type.as_deref() == Some("tool_search_call");
    VisibleItem {
        kind: "tool_result".to_owned(),
        content: if search {
            json!({"status":"completed","execution":"client","tools":[]})
        } else {
            json!({"output":"aborted"})
        },
        item_id: None,
        turn_id: call.turn_id.clone(),
        call_id: call.call_id.clone(),
        raw_type: Some(if search {
            "tool_search_output".to_owned()
        } else if call
            .raw_type
            .as_deref()
            .unwrap_or_default()
            .contains("custom_tool")
        {
            "custom_tool_call_output".to_owned()
        } else {
            "function_call_output".to_owned()
        }),
    }
}

fn wire_item(item: &VisibleItem) -> Option<Value> {
    let text = || content_text(&item.content).unwrap_or_default();
    match item.kind.as_str() {
        "user_message" => Some(message_with_content("user", &item.content)),
        "assistant_message" => Some(message_with_content("assistant", &item.content)),
        "compaction_summary" | "compaction_marker" => {
            let text = text();
            (!text.trim().is_empty())
                .then(|| message("user", &format!("{CHECKPOINT_PREFIX}\n\n{}", text.trim())))
        }
        "tool_call" | "tool_result" => {
            let mut value =
                item.content.as_object().cloned().unwrap_or_else(|| {
                    Map::from_iter([("output".to_owned(), item.content.clone())])
                });
            value.insert(
                "type".to_owned(),
                Value::String(item.raw_type.clone().unwrap_or_else(|| {
                    if item.kind == "tool_call" {
                        "function_call".to_owned()
                    } else {
                        "function_call_output".to_owned()
                    }
                })),
            );
            if let Some(call_id) = &item.call_id {
                value.insert("call_id".to_owned(), Value::String(call_id.clone()));
            }
            if let Some(item_id) = &item.item_id {
                value.insert("id".to_owned(), Value::String(item_id.clone()));
            }
            Some(Value::Object(value))
        }
        "standalone_tool_output" => {
            let mut value =
                item.content.as_object().cloned().unwrap_or_else(|| {
                    Map::from_iter([("output".to_owned(), item.content.clone())])
                });
            value.insert(
                "type".to_owned(),
                Value::String("function_call_output".to_owned()),
            );
            value.remove("call_id");
            Some(Value::Object(value))
        }
        _ => {
            let text = text();
            (!text.trim().is_empty()).then(|| {
                message(
                    "user",
                    &format!("Visible {}: {}", item.kind.replace('_', " "), text.trim()),
                )
            })
        }
    }
}

fn message_with_content(role: &str, content: &Value) -> Value {
    if let Value::Array(parts) = content {
        return json!({"type":"message","role":role,"content":parts});
    }
    if let Value::Object(value) = content
        && matches!(
            value.get("content"),
            Some(Value::String(_) | Value::Array(_))
        )
    {
        return message_with_content(role, &value["content"]);
    }
    message(role, &content_text(content).unwrap_or_default())
}

fn message(role: &str, text: &str) -> Value {
    let part_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({"type":"message","role":role,"content":[{"type":part_type,"text":text}]})
}

fn content_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(value) => Some(value.clone()),
        Value::Array(values) => Some(
            values
                .iter()
                .filter_map(content_text)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Object(value) => {
            for key in ["text", "message", "content", "output", "result"] {
                if let Some(text) = value.get(key).and_then(content_text)
                    && !text.is_empty()
                {
                    return Some(text);
                }
            }
            serde_json::to_string(value).ok()
        }
        value => Some(value.to_string()),
    }
}

fn input_items(root: &Map<String, Value>) -> Vec<Value> {
    match root.get("input") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(item)) => vec![Value::Object(item.clone())],
        Some(Value::String(value)) => vec![Value::String(value.clone())],
        _ => Vec::new(),
    }
}

fn header<'a>(
    incoming: &'a std::collections::BTreeMap<String, String>,
    wanted: &str,
) -> Option<&'a str> {
    incoming
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.trim())
}

fn string_alias(
    value: &Map<String, Value>,
    snake: &str,
    camel: &str,
) -> Result<Option<String>, HistoryError> {
    match optional_string(value, snake)? {
        Some(value) => Ok(Some(value)),
        None => optional_string(value, camel),
    }
}

fn optional_string(value: &Map<String, Value>, key: &str) -> Result<Option<String>, HistoryError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if !value.trim().is_empty() && value.len() <= 512 => {
            Ok(Some(value.trim().to_owned()))
        }
        Some(_) => Err(HistoryError::new(format!("invalid_{key}"))),
    }
}

fn legacy_codex_version(value: &str) -> bool {
    let parts = value
        .split('.')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>();
    matches!(parts.as_deref(), Ok([0, minor, patch]) if (*minor, *patch) <= (154, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let body = json!({"input":[{"type":"compaction","encrypted_content":"emp1:U3VtbWFyeSB0ZXh0Lg=="}]});
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
}
