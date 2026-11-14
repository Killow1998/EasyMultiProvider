//! Google generateContent conversion for Vertex connections.

use super::*;
use serde_json::json;

pub(crate) fn request(chat: &Value) -> Result<Value, RouterError> {
    let mut contents = Vec::new();
    let mut system = Vec::new();
    let mut names = HashMap::new();
    let mut messages = chat["messages"]
        .as_array()
        .expect("projected messages")
        .iter()
        .peekable();
    while let Some(message) = messages.next() {
        let role = message["role"].as_str().expect("projected role");
        if role == "tool" {
            let mut responses = Vec::new();
            let mut current = Some(message);
            while let Some(result) = current {
                let id = result["tool_call_id"]
                    .as_str()
                    .expect("projected tool result ID");
                let (name, order) = names.get(id).ok_or_else(|| {
                    invalid_request("Gemini tool result has no corresponding call")
                })?;
                responses.push((*order, json!({"functionResponse":{"name":name,"response":{"result":result["content"]}}})));
                current = if messages.peek().is_some_and(|next| next["role"] == "tool") {
                    messages.next()
                } else {
                    None
                };
            }
            responses.sort_by_key(|(order, _)| *order);
            contents.push(json!({"role":"user","parts":responses.into_iter().map(|(_, part)| part).collect::<Vec<_>>() }));
            continue;
        }
        let mut parts = content_parts(&message["content"])?;
        if role == "system" || role == "developer" {
            system.extend(parts);
            continue;
        }
        if let Some(calls) = message["tool_calls"].as_array() {
            for call in calls {
                let id = call["id"].as_str().expect("projected tool ID");
                let function = &call["function"];
                let name = function["name"].as_str().expect("projected tool name");
                let args: Value = serde_json::from_str(
                    function["arguments"].as_str().expect("projected arguments"),
                )
                .map_err(|_| invalid_request("invalid Gemini tool arguments"))?;
                names.insert(id.to_owned(), (name.to_owned(), names.len()));
                let mut part = json!({"functionCall":{"name":name,"args":args}});
                if let Some(signature) = call.pointer("/extra_content/google/thought_signature") {
                    part["thoughtSignature"] = signature.clone();
                }
                parts.push(part);
            }
        }
        if !parts.is_empty() {
            contents
                .push(json!({"role":if role == "assistant" {"model"} else {"user"},"parts":parts}));
        }
    }
    let mut request = json!({"contents":contents});
    if !system.is_empty() {
        request["systemInstruction"] = json!({"parts":system});
    }
    if let Some(tools) = chat["tools"].as_array() {
        let declarations: Vec<_> = tools
            .iter()
            .map(|tool| {
                let function = &tool["function"];
                let mut declaration = json!({"name":function["name"]});
                if !function["description"].is_null() {
                    declaration["description"] = function["description"].clone();
                }
                if !function["parameters"].is_null() {
                    declaration["parametersJsonSchema"] = function["parameters"].clone();
                }
                declaration
            })
            .collect();
        if !declarations.is_empty() {
            request["tools"] = json!([{"functionDeclarations":declarations}]);
        }
    }
    if let Some(choice) = chat.get("tool_choice") {
        let (mode, name) = match choice.as_str() {
            Some("none") => ("NONE", None),
            Some("required") => ("ANY", None),
            Some(_) => ("AUTO", None),
            None => (
                "ANY",
                choice.pointer("/function/name").and_then(Value::as_str),
            ),
        };
        request["toolConfig"]["functionCallingConfig"]["mode"] = json!(mode);
        if let Some(name) = name {
            request["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"] = json!([name]);
        }
    }
    for (from, to) in [
        ("max_tokens", "maxOutputTokens"),
        ("max_completion_tokens", "maxOutputTokens"),
        ("temperature", "temperature"),
        ("top_p", "topP"),
    ] {
        if let Some(value) = chat.get(from) {
            request["generationConfig"][to] = value.clone();
        }
    }
    if let Some(stop) = chat.get("stop") {
        request["generationConfig"]["stopSequences"] = if stop.is_string() {
            json!([stop])
        } else {
            stop.clone()
        };
    }
    match chat["response_format"]["type"].as_str() {
        Some("json_object") => {
            request["generationConfig"]["responseMimeType"] = json!("application/json")
        }
        Some("json_schema") => {
            request["generationConfig"]["responseMimeType"] = json!("application/json");
            request["generationConfig"]["responseJsonSchema"] =
                chat["response_format"]["json_schema"]["schema"].clone();
        }
        _ => {}
    }
    if let Some(effort) = chat["reasoning_effort"].as_str() {
        let level = match effort {
            "minimal" | "low" => "LOW",
            "medium" => "MEDIUM",
            "xhigh"
                if chat["model"].as_str().is_some_and(|model| {
                    model.strip_prefix("google/").unwrap_or(model) == "gemini-4-hydrogen"
                }) =>
            {
                "XHIGH"
            }
            "high" | "xhigh" => "HIGH",
            _ => "",
        };
        if !level.is_empty()
            && chat["model"]
                .as_str()
                .is_some_and(|model| model.contains("gemini-3") || model.contains("gemini-4"))
        {
            request["generationConfig"]["thinkingConfig"] =
                json!({"thinkingLevel":level,"includeThoughts":true});
        }
    }
    Ok(request)
}

fn content_parts(content: &Value) -> Result<Vec<Value>, RouterError> {
    if content.is_null() {
        return Ok(Vec::new());
    }
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"text":text})]);
    }
    let mut parts = Vec::new();
    for part in content
        .as_array()
        .ok_or_else(|| invalid_request("invalid Gemini message content"))?
    {
        match part["type"].as_str() {
            Some("text") => parts.push(json!({"text":part["text"]})),
            Some("image_url") => {
                let url = part
                    .pointer("/image_url/url")
                    .and_then(Value::as_str)
                    .expect("projected image URL");
                let Some((mime, data)) = url
                    .strip_prefix("data:")
                    .and_then(|value| value.split_once(";base64,"))
                else {
                    return Err(invalid_request(
                        "Gemini image input must be a base64 data URL",
                    ));
                };
                parts.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
            }
            Some("input_audio") => {
                let audio = &part["input_audio"];
                let format = audio["format"].as_str().expect("projected audio format");
                let mime = if format == "mp3" { "mpeg" } else { format };
                parts.push(
                    json!({"inlineData":{"mimeType":format!("audio/{mime}"),"data":audio["data"]}}),
                );
            }
            _ => return Err(invalid_request("unsupported Gemini message content")),
        }
    }
    Ok(parts)
}

#[derive(Debug)]
pub(crate) struct Stream {
    id: String,
    calls: usize,
    finished: bool,
}

impl Stream {
    pub(crate) fn new(response_id: &str) -> Self {
        Self {
            id: response_id.to_owned(),
            calls: 0,
            finished: false,
        }
    }

    pub(crate) fn finished(&self) -> bool {
        self.finished
    }

    pub(crate) fn push(&mut self, value: &Value) -> Result<Vec<Value>, RouterError> {
        if value.pointer("/promptFeedback/blockReason").is_some() {
            return Err(invalid_request("Gemini blocked the request"));
        }
        let mut chunks = Vec::new();
        if let Some(parts) = value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
        {
            for part in parts {
                let delta = if let Some(call) = part.get("functionCall") {
                    let name = call["name"]
                        .as_str()
                        .filter(|name| !name.is_empty())
                        .ok_or_else(|| {
                            invalid_request("Gemini returned a tool call without a name")
                        })?;
                    let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
                    if !args.is_object() {
                        return Err(invalid_request("Gemini returned invalid tool arguments"));
                    }
                    let id = call
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            format!(
                                "call_{}_{}",
                                self.id.trim_start_matches("resp_"),
                                self.calls
                            )
                        });
                    let mut projected = json!({"index":self.calls,"id":id,"type":"function","function":{
                        "name":name,"arguments":args.to_string()}});
                    if let Some(signature) = part.get("thoughtSignature") {
                        projected["extra_content"] =
                            json!({"google":{"thought_signature":signature}});
                    }
                    self.calls += 1;
                    json!({"tool_calls":[projected]})
                } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if part["thought"] == true {
                        json!({"reasoning_content":text})
                    } else {
                        json!({"content":text})
                    }
                } else {
                    continue;
                };
                chunks.push(json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}));
            }
        }
        if let Some(reason) = value
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
        {
            self.finished = true;
            let finish = match reason {
                "MAX_TOKENS" => "length",
                "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => "content_filter",
                "STOP" if self.calls > 0 => "tool_calls",
                "STOP" => "stop",
                _ => return Err(invalid_request("Gemini generation stopped unexpectedly")),
            };
            chunks.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]}));
        }
        if let Some(usage) = value.get("usageMetadata") {
            let mut mapped = json!({});
            if let Some(tokens) = usage.get("promptTokenCount") {
                mapped["prompt_tokens"] = tokens.clone();
            }
            let tokens = usage.get("candidatesTokenCount").and_then(Value::as_u64);
            let thoughts = usage.get("thoughtsTokenCount").and_then(Value::as_u64);
            if tokens.is_some() || thoughts.is_some() {
                mapped["completion_tokens"] = json!(tokens.unwrap_or(0) + thoughts.unwrap_or(0));
            }
            if let Some(tokens) = usage.get("totalTokenCount") {
                mapped["total_tokens"] = tokens.clone();
            }
            if let Some(tokens) = usage.get("cachedContentTokenCount") {
                mapped["prompt_tokens_details"]["cached_tokens"] = tokens.clone();
            }
            if let Some(tokens) = usage.get("thoughtsTokenCount") {
                mapped["completion_tokens_details"]["reasoning_tokens"] = tokens.clone();
            }
            chunks.push(json!({"choices":[],"usage":mapped}));
        }
        if let Some(model) = value.get("modelVersion").and_then(Value::as_str) {
            if chunks.is_empty() {
                chunks.push(json!({"choices":[]}));
            }
            chunks[0]["model"] = json!(model);
        }
        Ok(chunks)
    }
}

pub(crate) fn complete(value: &Value, response_id: &str) -> Result<Value, RouterError> {
    let mut stream = Stream::new(response_id);
    let mut message = json!({"role":"assistant","content":""});
    let mut reason = Value::Null;
    let mut usage = Value::Null;
    for chunk in stream.push(value)? {
        if let Some(delta) = chunk.pointer("/choices/0/delta") {
            for field in ["content", "reasoning_content"] {
                if let Some(text) = delta[field].as_str() {
                    let previous = message[field].as_str().unwrap_or_default();
                    message[field] = json!(format!("{previous}{text}"));
                }
            }
            if let Some(calls) = delta["tool_calls"].as_array() {
                if !message["tool_calls"].is_array() {
                    message["tool_calls"] = json!([]);
                }
                message["tool_calls"]
                    .as_array_mut()
                    .expect("tool calls array")
                    .extend(calls.iter().cloned());
            }
        }
        if let Some(value) = chunk
            .pointer("/choices/0/finish_reason")
            .filter(|value| !value.is_null())
        {
            reason = value.clone();
        }
        if let Some(value) = chunk.get("usage") {
            usage = value.clone();
        }
    }
    if !stream.finished() {
        return Err(invalid_request("Gemini response did not complete"));
    }
    let mut chat = json!({"choices":[{"index":0,"message":message,"finish_reason":reason}]});
    if let Some(model) = value.get("modelVersion").and_then(Value::as_str) {
        chat["model"] = json!(model);
    }
    if !usage.is_null() {
        chat["usage"] = usage;
    }
    Ok(chat)
}
