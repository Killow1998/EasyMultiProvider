//! JSON token estimation and image redaction, without cloning plain text payloads.
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::io::{self, Write};

const IMAGE_INPUT_TOKEN_ESTIMATE: u64 = 4096;
const IMAGE_TYPES: &[&str] = &["input_image", "output_image", "image", "image_url"];

pub fn estimate_json_tokens(value: &Value) -> Option<u64> {
    match contains_image(value, 0) {
        Some(false) => {
            let bytes = serialized_json_bytes(value)?;
            Some(tokens_from_bytes(bytes, 0))
        }
        Some(true) | None => materialized_estimate_json_tokens(value),
    }
}

pub(super) fn estimate_protocol_payload_tokens(payload: &Value, protocol: &str) -> Option<u64> {
    let root = payload.as_object()?;
    let fields = protocol_fields(protocol)?;
    let mut images = false;
    for field in fields {
        if let Some(value) = root.get(*field) {
            match contains_image(value, 1) {
                Some(false) => {}
                Some(true) | None => {
                    images = true;
                    break;
                }
            }
        }
    }
    if images {
        return payload_view(payload, protocol)
            .and_then(|view| materialized_estimate_json_tokens(&view));
    }
    let bytes = serialized_json_bytes(&SelectedProtocolFields { root, fields })?;
    Some(tokens_from_bytes(bytes, 0))
}

fn tokens_from_bytes(bytes: u64, image_count: u64) -> u64 {
    let text = if bytes == 0 {
        0
    } else {
        bytes.div_ceil(2).max(1)
    };
    text.saturating_add(image_count.saturating_mul(IMAGE_INPUT_TOKEN_ESTIMATE))
}

fn materialized_estimate_json_tokens(value: &Value) -> Option<u64> {
    let mut image_count = 0_u64;
    let redacted = redact_images_with(value, &mut image_count, 0, &[])?;
    let bytes = serde_json::to_vec(&redacted).ok()?.len() as u64;
    Some(tokens_from_bytes(bytes, image_count))
}

fn serialized_json_bytes(value: &impl Serialize) -> Option<u64> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value).ok()?;
    u64::try_from(writer.bytes).ok()
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("serialized JSON length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct SelectedProtocolFields<'a> {
    root: &'a Map<String, Value>,
    fields: &'static [&'static str],
}

impl Serialize for SelectedProtocolFields<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let count = self
            .root
            .keys()
            .filter(|key| self.fields.contains(&key.as_str()))
            .count();
        let mut object = serializer.serialize_map(Some(count))?;
        for (key, value) in self.root {
            if self.fields.contains(&key.as_str()) {
                object.serialize_entry(key, value)?;
            }
        }
        object.end()
    }
}

fn contains_image(value: &Value, depth: usize) -> Option<bool> {
    if depth > 128 {
        return None;
    }
    match value {
        Value::Array(values) => {
            for value in values {
                if contains_image(value, depth + 1)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        Value::Object(values) => {
            if matches!(
                values.get("type").and_then(Value::as_str),
                Some(kind) if IMAGE_TYPES.contains(&kind)
            ) {
                return Some(true);
            }
            for value in values.values() {
                if contains_image(value, depth + 1)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        _ => Some(false),
    }
}

/// Serialized size of one JSON value in the token estimator's terms: bytes
/// after image redaction plus the number of redacted images.
#[derive(Clone, Copy, Default)]
pub(super) struct JsonCost {
    bytes: u64,
    images: u64,
}

/// Mirrors `estimate_json_tokens` for a value nested `depth` levels deep, so
/// per-item costs sum to the estimate of the enclosing array.
pub(super) fn json_cost(value: &Value, depth: usize) -> Option<JsonCost> {
    match contains_image(value, depth)? {
        false => Some(JsonCost {
            bytes: serialized_json_bytes(value)?,
            images: 0,
        }),
        true => {
            let mut images = 0_u64;
            let redacted = redact_images_with(value, &mut images, depth, &[])?;
            Some(JsonCost {
                bytes: serialized_json_bytes(&redacted)?,
                images,
            })
        }
    }
}

/// Token estimate of a request whose `input` array grows one item at a time,
/// computed without re-serialising the request for every candidate.
#[derive(Clone, Copy)]
pub(super) struct IncrementalEstimate {
    total: Option<JsonCost>,
    items: u64,
}

impl IncrementalEstimate {
    /// `base` is the request with only its `fixed` always-present input items.
    pub(super) fn new(base: &Value, fixed: usize) -> Self {
        Self {
            total: json_cost(base, 0),
            items: fixed as u64,
        }
    }

    fn added(&self, costs: &[JsonCost]) -> Option<JsonCost> {
        let mut total = self.total?;
        let mut items = self.items;
        for cost in costs {
            // One separating comma per item beyond the first.
            let comma = u64::from(items > 0);
            total.bytes = total.bytes.checked_add(cost.bytes)?.checked_add(comma)?;
            total.images = total.images.saturating_add(cost.images);
            items += 1;
        }
        Some(total)
    }

    pub(super) fn tokens_with(&self, costs: &[JsonCost]) -> Option<u64> {
        self.added(costs)
            .map(|total| tokens_from_bytes(total.bytes, total.images))
    }

    pub(super) fn extend(&mut self, costs: &[JsonCost]) {
        self.total = self.added(costs);
        self.items = self.items.saturating_add(costs.len() as u64);
    }
}

fn payload_view(payload: &Value, protocol: &str) -> Option<Value> {
    let root = payload.as_object()?;
    let fields = protocol_fields(protocol)?;
    Some(Value::Object(
        fields
            .iter()
            .filter_map(|field| {
                root.get(*field)
                    .cloned()
                    .map(|value| ((*field).to_owned(), value))
            })
            .collect(),
    ))
}

fn protocol_fields(protocol: &str) -> Option<&'static [&'static str]> {
    Some(match protocol {
        "responses" => &["input", "instructions", "tools", "text", "response_format"],
        "chat_completions" => &["messages", "tools", "response_format"],
        "anthropic_messages" => &["system", "messages", "tools"],
        _ => return None,
    })
}

fn redact_images_with(
    value: &Value,
    images: &mut u64,
    depth: usize,
    redacted_keys: &[&str],
) -> Option<Value> {
    if depth > 128 {
        return None;
    }
    match value {
        Value::Array(values) => Some(Value::Array(
            values
                .iter()
                .map(|value| redact_images_with(value, images, depth + 1, &[]))
                .collect::<Option<Vec<_>>>()?,
        )),
        Value::Object(value) => {
            let image = matches!(
                value.get("type").and_then(Value::as_str),
                Some(kind) if IMAGE_TYPES.contains(&kind)
            );
            if image {
                *images = images.saturating_add(1);
            }
            Some(Value::Object(
                value
                    .iter()
                    .map(|(key, item)| {
                        let item = if redacted_keys.contains(&key.as_str())
                            || image && matches!(key.as_str(), "data" | "image_data")
                        {
                            Value::String("<image>".to_owned())
                        } else if image && key == "image_url" {
                            if item.is_object() {
                                redact_images_with(item, images, depth + 1, &["url"])?
                            } else {
                                Value::String("<image>".to_owned())
                            }
                        } else if image && key == "source" && item.is_object() {
                            redact_images_with(item, images, depth + 1, &["data", "url"])?
                        } else {
                            redact_images_with(item, images, depth + 1, &[])?
                        };
                        Some((key.clone(), item))
                    })
                    .collect::<Option<Map<_, _>>>()?,
            ))
        }
        value => Some(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn streaming_token_estimate_matches_materialized_text_and_nested_values() {
        let value = json!({
            "text":"ordinary ".repeat(64 * 1024),
            "unicode":"snowman ☃ and globe 🌎",
            "escaped":"quote \" slash \\ newline\n tab\t",
            "nested":[{"items":[true,false,null,-5,2.5]}]
        });
        assert_eq!(
            estimate_json_tokens(&value),
            materialized_estimate_json_tokens(&value)
        );
    }

    #[test]
    fn image_estimate_fallback_matches_materialized_redaction_shapes() {
        let value = json!([
            {"type":"input_image","data":"private","image_url":{"url":"https://example.test/a.png","detail":"high"}},
            {"type":"image","source":{"type":"base64","data":"private","url":"private","media_type":"image/png"}},
            {"type":"image_url","image_url":"https://example.test/b.png"},
            {"type":"message","content":[{"type":"output_image","image_data":"private"}]}
        ]);
        assert_eq!(
            estimate_json_tokens(&value),
            materialized_estimate_json_tokens(&value)
        );
    }

    #[test]
    fn protocol_field_streaming_matches_materialized_view_for_all_protocols() {
        let payload = json!({
            "input":[{"type":"message","role":"user","content":"responses"}],
            "instructions":"instruction",
            "tools":[{"type":"function","name":"search","parameters":{"type":"object"}}],
            "text":{"format":{"type":"json_schema"}},
            "response_format":{"type":"json_object"},
            "messages":[{"role":"user","content":[{"type":"image","data":"private"}]}],
            "system":[{"type":"text","text":"anthropic"}],
            "ignored":{"type":"image","data":"must not be counted"}
        });
        let plain_payload = json!({
            "input":"responses selected",
            "instructions":"instruction",
            "tools":[{"type":"function","name":"search","parameters":{"type":"object"}}],
            "text":{"format":{"type":"json_schema"}},
            "response_format":{"type":"json_object"},
            "messages":[{"role":"user","content":"chat selected"}],
            "system":[{"type":"text","text":"anthropic selected"}],
            "ignored":{"type":"image","data":"must not be counted"}
        });
        for protocol in ["responses", "chat_completions", "anthropic_messages"] {
            let expected = payload_view(&payload, protocol)
                .and_then(|view| materialized_estimate_json_tokens(&view));
            assert_eq!(
                estimate_protocol_payload_tokens(&payload, protocol),
                expected
            );
            let plain_expected = payload_view(&plain_payload, protocol)
                .and_then(|view| materialized_estimate_json_tokens(&view));
            assert_eq!(
                estimate_protocol_payload_tokens(&plain_payload, protocol),
                plain_expected
            );
        }
        assert_eq!(estimate_protocol_payload_tokens(&payload, "unknown"), None);
    }

    #[test]
    fn streaming_token_estimate_preserves_the_materialized_depth_limit() {
        let mut within_limit = json!("leaf");
        for _ in 0..128 {
            within_limit = json!([within_limit]);
        }
        assert_eq!(
            estimate_json_tokens(&within_limit),
            materialized_estimate_json_tokens(&within_limit)
        );
        assert!(estimate_json_tokens(&within_limit).is_some());

        let too_deep = json!([within_limit]);
        assert_eq!(
            estimate_json_tokens(&too_deep),
            materialized_estimate_json_tokens(&too_deep)
        );
        assert!(estimate_json_tokens(&too_deep).is_none());
    }
}
