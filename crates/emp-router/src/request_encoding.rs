//! Reserve the projected JSON size before writing large conversation histories.

use serde_json::Value;

fn unescaped_json_size(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().len(),
        Value::String(text) => text.len().saturating_add(2),
        Value::Array(items) => items.iter().fold(
            2_usize.saturating_add(items.len().saturating_sub(1)),
            |size, item| size.saturating_add(unescaped_json_size(item)),
        ),
        Value::Object(fields) => fields.iter().fold(
            2_usize.saturating_add(fields.len().saturating_sub(1)),
            |size, (key, item)| {
                size.saturating_add(key.len())
                    .saturating_add(3)
                    .saturating_add(unescaped_json_size(item))
            },
        ),
    }
}

pub(super) fn encode_projected_request(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    // serde_json::to_vec grows from a small initial buffer. A 128 MiB history
    // can leave a nearly 256 MiB buffer live beside both parsed JSON trees.
    // This lower bound is exact for ordinary text and safely grows for escapes.
    let mut encoded = Vec::with_capacity(unescaped_json_size(value));
    serde_json::to_writer(&mut encoded, value)?;
    Ok(encoded)
}
