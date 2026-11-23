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

pub(super) fn encode_projected_request(value: &Value) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(unescaped_json_size(value));
    serde_json::to_writer(&mut encoded, value).expect("JSON value into memory");
    encoded
}
