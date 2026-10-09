//! Pull-based synthesis for a complete Responses JSON reply.
//! Keep one source response and construct only the event the consumer requests.
//! Already-delivered bytes consume no space here. Validate the source once;
//! transport writes provide backpressure, and dropping the iterator cancels
//! synthesis without producing the remaining events.
use super::{ProjectionIds, RouterError};
use emp_protocol::portable_responses::validate_responses_body;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy)]
enum Phase {
    Created,
    ItemAdded,
    Part(u8),
    Arguments,
    ItemDone,
    Terminal,
    Finished,
}

#[derive(Debug)]
pub struct ResponseEvents {
    response: Value,
    message_id: String,
    output_index: usize,
    content_index: usize,
    phase: Phase,
}

pub fn response_json_stream_events(
    mut response: Value,
    ids: &ProjectionIds,
    validate_output_items: bool,
) -> Result<ResponseEvents, RouterError> {
    validate_responses_body(&response, validate_output_items).map_err(RouterError::from)?;
    let root = response
        .as_object_mut()
        .expect("validated Responses object");
    root.entry("id").or_insert_with(|| json!(ids.response));
    root.entry("object").or_insert_with(|| json!("response"));
    if root["output"]
        .as_array()
        .expect("validated output")
        .is_empty()
        && let Some(text) = root
            .get("output_text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    {
        root.insert(
            "output".into(),
            json!([{
                "id":ids.message,"type":"message","status":"completed","role":"assistant",
                "content":[{"type":"output_text","text":text,"annotations":[]}]
            }]),
        );
    }
    Ok(ResponseEvents {
        response,
        message_id: ids.message.clone(),
        output_index: 0,
        content_index: 0,
        phase: Phase::Created,
    })
}

impl ResponseEvents {
    fn item(&self) -> Option<&Map<String, Value>> {
        self.response["output"]
            .as_array()?
            .get(self.output_index)?
            .as_object()
    }

    fn item_id(&self, item: &Map<String, Value>) -> Value {
        item.get("id").cloned().unwrap_or_else(|| {
            json!(if self.output_index == 0 {
                self.message_id.clone()
            } else {
                format!("item_{}", self.output_index)
            })
        })
    }

    fn item_event(&self, event_type: &str, status: &str) -> Value {
        let source = self.item().expect("current item");
        let mut item = source.clone();
        item.insert("id".into(), self.item_id(source));
        if item.get("type").and_then(Value::as_str) != Some("compaction") {
            item.insert("status".into(), json!(status));
        }
        json!({"type":event_type,"output_index":self.output_index,"item":item})
    }
}

impl Iterator for ResponseEvents {
    type Item = Value;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.phase {
                Phase::Created => {
                    // Copy metadata without first cloning the entire output array.
                    let mut created: Map<String, Value> = self
                        .response
                        .as_object()
                        .unwrap()
                        .iter()
                        .filter(|(key, _)| key.as_str() != "output")
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect();
                    created.insert("status".into(), json!("in_progress"));
                    created.insert("output".into(), json!([]));
                    self.phase = Phase::ItemAdded;
                    return Some(json!({"type":"response.created","response":created}));
                }
                Phase::ItemAdded => {
                    let Some(item) = self.item() else {
                        self.phase = Phase::Terminal;
                        continue;
                    };
                    let next = match item
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message")
                    {
                        "message" => Phase::Part(0),
                        "function_call" => Phase::Arguments,
                        _ => Phase::ItemDone,
                    };
                    let event = self.item_event("response.output_item.added", "in_progress");
                    self.phase = next;
                    self.content_index = 0;
                    return Some(event);
                }
                Phase::Part(stage) => {
                    let item = self.item().expect("current message");
                    let Some(part) = item
                        .get("content")
                        .and_then(Value::as_array)
                        .and_then(|parts| parts.get(self.content_index))
                    else {
                        self.phase = Phase::ItemDone;
                        continue;
                    };
                    let field = match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => "text",
                        Some("refusal") => "refusal",
                        _ => {
                            self.content_index += 1;
                            continue;
                        }
                    };
                    let kind = part["type"].as_str().unwrap();
                    let mut event = json!({"item_id":self.item_id(item),"output_index":self.output_index,"content_index":self.content_index});
                    let fields = event.as_object_mut().unwrap();
                    match stage {
                        0 => {
                            let mut empty = part.clone();
                            empty[field] = json!("");
                            fields.insert("type".into(), json!("response.content_part.added"));
                            fields.insert("part".into(), empty);
                        }
                        1 | 2 => {
                            fields.insert(
                                "type".into(),
                                json!(format!(
                                    "response.{kind}.{}",
                                    if stage == 1 { "delta" } else { "done" }
                                )),
                            );
                            fields.insert(
                                if stage == 1 { "delta" } else { field }.into(),
                                part.get(field).cloned().unwrap_or_else(|| json!("")),
                            );
                        }
                        _ => {
                            fields.insert("type".into(), json!("response.content_part.done"));
                            fields.insert("part".into(), part.clone());
                        }
                    }
                    if stage == 3 {
                        self.content_index += 1;
                    }
                    self.phase = Phase::Part((stage + 1) % 4);
                    return Some(event);
                }
                Phase::Arguments => {
                    let item = self.item().expect("current tool");
                    let event = json!({"type":"response.function_call_arguments.done","item_id":self.item_id(item),
                        "output_index":self.output_index,"arguments":item.get("arguments").cloned().unwrap_or_else(|| json!("{}"))});
                    self.phase = Phase::ItemDone;
                    return Some(event);
                }
                Phase::ItemDone => {
                    let event = self.item_event("response.output_item.done", "completed");
                    self.output_index += 1;
                    self.phase = Phase::ItemAdded;
                    return Some(event);
                }
                Phase::Terminal => {
                    let kind = match self.response["status"].as_str().expect("validated status") {
                        "completed" => "response.completed",
                        "incomplete" => "response.incomplete",
                        "failed" => "response.failed",
                        _ => unreachable!("validated status"),
                    };
                    self.phase = Phase::Finished;
                    return Some(
                        json!({"type":kind,"response":std::mem::take(&mut self.response)}),
                    );
                }
                Phase::Finished => return None,
            }
        }
    }
}

impl std::iter::FusedIterator for ResponseEvents {}
