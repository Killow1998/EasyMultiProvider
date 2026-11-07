//! Stream only new CLI records; do not rescan or retain its context log.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

#[derive(Default)]
pub(super) struct Metrics {
    messages: BTreeMap<String, Receipt>,
    latest_text: String,
    position: u64,
    pending: Vec<u8>,
    skip_line: bool,
}

struct Receipt {
    usage: Value,
    model: Option<String>,
}

impl Metrics {
    fn observe(&mut self, mut message: Value) {
        if message["role"] != "assistant" {
            return;
        }
        self.latest_text = message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(id) = message["id"].as_str().map(str::to_owned) {
            self.messages.insert(
                id,
                Receipt {
                    model: message["model"].as_str().map(str::to_owned),
                    usage: message
                        .get_mut("usage")
                        .map(Value::take)
                        .unwrap_or_default(),
                },
            );
        }
    }

    pub(super) fn response(&mut self, raw: &[u8]) -> Result<(), emp_router::RouterError> {
        if let Ok(message) = serde_json::from_slice::<Value>(raw) {
            if message["type"] == "error" {
                return Err(emp_router::upstream_error(502, raw, None));
            }
            self.observe(message);
            return Ok(());
        }
        let mut message = Value::Null;
        for line in raw.split(|byte| *byte == b'\n') {
            let Some(data) = line.strip_prefix(b"data:") else {
                continue;
            };
            let Ok(event) = serde_json::from_slice::<Value>(data) else {
                continue;
            };
            match event["type"].as_str() {
                Some("error") => {
                    self.observe(message);
                    return Err(emp_router::upstream_error(502, data, None));
                }
                Some("message_start") => message = event["message"].clone(),
                Some("content_block_start") => {
                    if let Some(blocks) = message["content"].as_array_mut() {
                        blocks.push(event["content_block"].clone());
                    }
                }
                Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                    if let Some(block) = event["index"]
                        .as_u64()
                        .and_then(|index| message["content"].get_mut(index as usize))
                    {
                        let mut text = block["text"].as_str().unwrap_or_default().to_owned();
                        text.push_str(event["delta"]["text"].as_str().unwrap_or_default());
                        block["text"] = json!(text);
                    }
                }
                Some("message_delta") => {
                    message["stop_reason"] = event["delta"]["stop_reason"].clone();
                    if let Some(usage) = event["usage"].as_object() {
                        if !message["usage"].is_object() {
                            message["usage"] = json!({});
                        }
                        message["usage"]
                            .as_object_mut()
                            .expect("usage object")
                            .extend(usage.clone());
                    }
                }
                _ => {}
            }
        }
        self.observe(message);
        Ok(())
    }

    pub(super) fn drain(&mut self, config: &Path, session: &str) -> std::io::Result<()> {
        let Some(path) = std::fs::read_dir(config.join("projects"))
            .ok()
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join(format!("{session}.jsonl")))
            .find(|path| path.is_file())
        else {
            return Ok(());
        };
        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(self.position))?;
        let mut buffer = [0; 65536];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            self.position += count as u64;
            for byte in &buffer[..count] {
                if *byte == b'\n' {
                    if !self.skip_line
                        && let Ok(record) = serde_json::from_slice::<Value>(&self.pending)
                        && record["type"] == "assistant"
                    {
                        self.observe(record["message"].clone());
                    }
                    self.pending.clear();
                    self.skip_line = false;
                } else if !self.skip_line {
                    if self.pending.len() < 4 * 1024 * 1024 {
                        self.pending.push(*byte);
                    } else {
                        self.pending.clear();
                        self.skip_line = true;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn text(&self) -> &str {
        &self.latest_text
    }

    pub(super) fn output(&mut self, proposal: Value) -> Value {
        let mut usage = serde_json::Map::new();
        for field in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ] {
            let values = self.messages.values().try_fold(0u64, |sum, receipt| {
                Some(sum + receipt.usage[field].as_u64()?)
            });
            if let Some(total) = values.filter(|_| !self.messages.is_empty()) {
                usage.insert(field.into(), json!(total));
            }
        }
        let mut creation = serde_json::Map::new();
        for field in ["ephemeral_5m_input_tokens", "ephemeral_1h_input_tokens"] {
            let values = self.messages.values().try_fold(0u64, |sum, receipt| {
                Some(sum + receipt.usage["cache_creation"][field].as_u64()?)
            });
            if let Some(total) = values.filter(|_| !self.messages.is_empty()) {
                creation.insert(field.into(), json!(total));
            }
        }
        if !creation.is_empty() {
            usage.insert("cache_creation".into(), Value::Object(creation));
        }
        let declarations = self
            .messages
            .values()
            .filter_map(|receipt| receipt.model.as_deref())
            .map(|model| json!({"model":model,"source":"upstream_body","terminal":true}))
            .collect::<Vec<_>>();
        self.messages.clear();
        self.latest_text.clear();
        json!({"structured_output":proposal,"usage":if usage.is_empty() {Value::Null} else {Value::Object(usage)},"_emp_model_observation":{"model_declarations":declarations}})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipts_deduplicate_keep_unknown_usage_and_preserve_usage_before_sse_error() {
        let mut metrics = Metrics::default();
        let receipt = json!({"role":"assistant","id":"first","model":"fixture",
            "usage":{"input_tokens":0,"output_tokens":3,"cache_creation_input_tokens":2,
                "cache_creation":{"ephemeral_1h_input_tokens":2}},
            "content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"first answer"}]});
        for _ in 0..2 {
            metrics
                .response(&serde_json::to_vec(&receipt).unwrap())
                .unwrap();
        }
        let output = metrics.output(json!({"answer":metrics.text(),"tool_calls":[]}));
        assert_eq!(output["structured_output"]["answer"], "first answer");
        assert_eq!(output["usage"]["input_tokens"], 0);
        assert_eq!(output["usage"]["output_tokens"], 3);
        assert_eq!(
            output["usage"]["cache_creation"]["ephemeral_1h_input_tokens"],
            2
        );
        assert!(output["usage"]["cache_read_input_tokens"].is_null());
        assert!(!output.to_string().contains("private"));
        assert!(metrics.output(json!({}))["usage"].is_null());

        for receipt in [
            json!({"role":"assistant","id":"one","usage":{"input_tokens":4,"output_tokens":3}}),
            json!({"role":"assistant","id":"two","usage":{"input_tokens":7}}),
        ] {
            metrics
                .response(&serde_json::to_vec(&receipt).unwrap())
                .unwrap();
        }
        let output = metrics.output(json!({}));
        assert_eq!(output["usage"]["input_tokens"], 11);
        assert!(output["usage"]["output_tokens"].is_null());

        let error = metrics.response(b"data: {\"type\":\"message_start\",\"message\":{\"id\":\"partial\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":9}}}\n\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n").unwrap_err();
        assert_eq!(error.status(), 502);
        assert_eq!(metrics.output(json!({}))["usage"]["input_tokens"], 9);
    }
}
