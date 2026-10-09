//! Facts collected before protocol/model projection; never affect forwarding.
use serde_json::{Value, json};
use std::time::Instant;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelObservation {
    first: Option<String>,
    header_model: Option<String>,
    terminal: Option<String>,
    declarations: Vec<Value>,
    conflict: bool,
    pub first_output: Option<Instant>,
    pub first_content: Option<Instant>,
}
impl ModelObservation {
    pub fn model(&self) -> Option<&str> {
        self.terminal
            .as_deref()
            .or(self.first.as_deref())
            .or(self.header_model.as_deref())
    }
    pub fn observe_model(&mut self, model: &str, source: &str, terminal: bool) {
        let model = model.trim();
        if model.is_empty() || model.len() > 512 || model.chars().any(char::is_control) {
            return;
        }
        if self.model().is_some_and(|previous| previous != model) {
            self.conflict = true;
        }
        if terminal {
            self.terminal = Some(model.into());
        } else if matches!(source, "openai-model" | "x-openai-model") {
            self.header_model.get_or_insert_with(|| model.into());
        } else if self.first.is_none() {
            self.first = Some(model.into());
        }
        let declaration = json!({"model":model,"source":source,"terminal":terminal});
        if !self.declarations.contains(&declaration) {
            if self.declarations.len() < 8 {
                self.declarations.push(declaration);
            } else if terminal {
                self.declarations[7] = declaration;
            }
        }
    }
    pub fn observe_headers<'a>(&mut self, headers: impl IntoIterator<Item = (&'a str, &'a str)>) {
        for (name, value) in headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "openai-model" | "x-openai-model"
            ) {
                self.observe_model(value, &name.to_ascii_lowercase(), false);
            }
        }
    }
    pub fn observe(&mut self, event: &Value, live: bool) {
        for headers in [&event["headers"], &event["response"]["headers"]] {
            if let Some(headers) = headers.as_object() {
                self.observe_headers(
                    headers.iter().filter_map(|(key, value)| {
                        value.as_str().map(|value| (key.as_str(), value))
                    }),
                );
            }
        }
        let kind = event["type"].as_str().unwrap_or("");
        let terminal = matches!(
            kind,
            "response.completed"
                | "response.done"
                | "response.failed"
                | "response.incomplete"
                | "response.cancelled"
                | "response.canceled"
        ) || matches!(
            event["status"].as_str(),
            Some("completed" | "failed" | "incomplete")
        ) || (kind == "message" && event["model"].is_string())
            || (kind.is_empty()
                && event.get("choices").is_some_and(|v| v.is_array())
                && event["object"] != "chat.completion.chunk");
        for (object, source) in [
            (event, "model"),
            (&event["response"], "response.model"),
            (&event["message"], "message.model"),
        ] {
            if let Some(model) = object["model"].as_str() {
                self.observe_model(model, source, terminal);
            }
        }
        if !live {
            return;
        }
        let content = matches!(
            kind,
            "response.output_text.delta"
                | "response.refusal.delta"
                | "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta"
        ) && event["delta"].as_str().is_some_and(|v| !v.is_empty());
        let block = kind == "response.output_item.added" || kind == "content_block_start";
        let chat = event["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = &choice["delta"];
                ["content", "reasoning_content", "reasoning"]
                    .iter()
                    .any(|key| delta[*key].as_str().is_some_and(|v| !v.is_empty()))
                    || delta["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
            })
        });
        let anthropic_delta = kind == "content_block_delta"
            && ["text", "thinking", "partial_json"]
                .iter()
                .any(|key| event["delta"][*key].as_str().is_some_and(|v| !v.is_empty()));
        let reasoning = matches!(
            kind,
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta"
        ) && event["delta"].as_str().is_some_and(|v| !v.is_empty());
        if content || block || chat || anthropic_delta || reasoning {
            self.first_output.get_or_insert_with(Instant::now);
        }
        if content
            || (kind == "content_block_delta"
                && ["text", "partial_json"]
                    .iter()
                    .any(|key| event["delta"][*key].as_str().is_some_and(|v| !v.is_empty())))
            || event["choices"].as_array().is_some_and(|choices| {
                choices.iter().any(|choice| {
                    choice["delta"]["content"]
                        .as_str()
                        .is_some_and(|v| !v.is_empty())
                        || choice["delta"]["tool_calls"]
                            .as_array()
                            .is_some_and(|calls| {
                                calls.iter().any(|call| {
                                    call["function"]["arguments"]
                                        .as_str()
                                        .is_some_and(|value| !value.is_empty())
                                })
                            })
                })
            })
        {
            self.first_content.get_or_insert_with(Instant::now);
        }
    }
    pub fn project(&self, sent: &str) -> Value {
        let status = if self.conflict {
            "conflict"
        } else {
            match self.model() {
                None => "missing",
                Some(model) if model == sent => "same",
                Some(_) => "different",
            }
        };
        let source = self
            .declarations
            .iter()
            .rev()
            .find(|value| value["model"].as_str() == self.model())
            .and_then(|value| value["source"].as_str());
        json!({"response_model":self.model(),"response_model_source":source,"model_name_status":status,"model_declarations":self.declarations})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_first_terminal_and_header_declarations_survive_projection() {
        let mut observation = ModelObservation::default();
        observation.observe_headers([("openai-model", "first"), ("Authorization", "private")]);
        observation.observe(
            &json!({"type":"response.created","response":{"model":"first"}}),
            true,
        );
        assert!(observation.first_output.is_none());
        observation.observe(
            &json!({"type":"response.output_item.added","item":{"type":"reasoning"}}),
            true,
        );
        assert!(observation.first_output.is_some());
        assert!(observation.first_content.is_none());
        observation.observe(
            &json!({"type":"response.completed","response":{"model":"last"}}),
            true,
        );
        let facts = observation.project("first");
        assert_eq!(facts["response_model"], "last");
        assert_eq!(facts["model_name_status"], "conflict");
        assert!(!facts.to_string().contains("private"));
        assert_eq!(
            ModelObservation::default().project("first")["model_name_status"],
            "missing"
        );
    }
}
