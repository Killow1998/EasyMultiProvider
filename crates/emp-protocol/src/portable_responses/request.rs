//! Top-level Responses request policy and portable request assembly.
use super::input::portable_input;
use super::tools::{portable_tools, raw_tools};
use super::{PortableProjectionError, error};
use serde_json::{Map, Value};
use url::Url;

const PORTABLE_TOP_LEVEL: &[&str] = &[
    "model",
    "instructions",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "temperature",
    "top_p",
    "stop",
    "max_output_tokens",
    "stream",
    "reasoning",
    "text",
    "truncation",
    "service_tier",
];
fn official_openai_responses(provider: &Map<String, Value>) -> bool {
    let Some(raw) = provider
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Ok(parsed) = Url::parse(raw) else {
        return false;
    };
    let path = parsed.path().trim_end_matches('/');
    parsed.scheme().eq_ignore_ascii_case("https")
        && parsed
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.openai.com"))
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.port().is_none_or(|port| port == 443)
        && matches!(path, "/v1" | "/v1/responses")
}

pub fn project_request(
    provider: &Map<String, Value>,
    body: &Value,
    preserve_reasoning_state: bool,
) -> Result<Value, PortableProjectionError> {
    let body = body
        .as_object()
        .ok_or_else(|| error(0, "input", "invalid_input"))?;
    if body.contains_key("previous_response_id") {
        return Err(error(
            0,
            "previous_response_id",
            "stateful_response_unsupported",
        ));
    }
    let mut projected = Map::new();
    for key in PORTABLE_TOP_LEVEL {
        if let Some(value) = body.get(*key) {
            projected.insert((*key).to_owned(), value.clone());
        }
    }
    if official_openai_responses(provider) {
        for key in ["store", "include", "prompt_cache_key"] {
            if let Some(value) = body.get(key) {
                projected.insert(key.to_owned(), value.clone());
            }
        }
    }
    let (input, hoisted) = portable_input(body.get("input"), preserve_reasoning_state)?;
    projected.insert("input".to_owned(), input);
    let mut instructions = Vec::new();
    match projected.get("instructions") {
        Some(Value::String(value)) => instructions.push(value.clone()),
        None | Some(Value::Null) => {}
        Some(_) => return Err(error(0, "instructions", "invalid_instruction_content")),
    }
    instructions.extend(hoisted);
    if !instructions.is_empty() {
        projected.insert(
            "instructions".to_owned(),
            Value::String(instructions.join("\n\n")),
        );
    }
    let tools = raw_tools(body)?;
    if tools.is_empty() {
        projected.remove("tools");
    } else {
        let tools = portable_tools(tools)?;
        if tools.is_empty() {
            projected.remove("tools");
        } else {
            projected.insert("tools".to_owned(), Value::Array(tools));
        }
    }
    if projected
        .get("tool_choice")
        .and_then(Value::as_object)
        .and_then(|choice| choice.get("type"))
        .and_then(Value::as_str)
        == Some("custom")
    {
        projected["tool_choice"]["type"] = Value::String("function".to_owned());
    }
    match projected.get("stream") {
        None | Some(Value::Null) => {
            projected.insert("stream".to_owned(), Value::Bool(false));
        }
        Some(Value::Bool(_)) => {}
        Some(_) => return Err(error(0, "stream", "invalid_stream")),
    }
    Ok(Value::Object(projected))
}

#[cfg(test)]
mod request_projection_tests {
    use super::{PORTABLE_TOP_LEVEL, project_request};
    use serde_json::{Map, Value, json};

    #[test]
    fn large_string_input_is_only_added_by_portable_input_and_output_is_unchanged() {
        assert!(!PORTABLE_TOP_LEVEL.contains(&"input"));
        let input = "large request body ".repeat(64 * 1024);
        let provider = Map::from_iter([
            ("protocol".to_owned(), Value::String("responses".to_owned())),
            ("auth_mode".to_owned(), Value::String("api_key".to_owned())),
        ]);
        let body = json!({
            "model":"provider/model",
            "instructions":"continue carefully",
            "input":input,
            "unknown":"not forwarded"
        });

        let projected = project_request(&provider, &body, false).expect("portable projection");
        assert_eq!(
            projected,
            json!({
                "model":"provider/model",
                "instructions":"continue carefully",
                "input":input,
                "stream":false
            })
        );
    }

    #[test]
    fn audio_input_is_forwarded_to_responses_providers() {
        let provider = Map::from_iter([
            ("protocol".to_owned(), Value::String("responses".to_owned())),
            ("auth_mode".to_owned(), Value::String("api_key".to_owned())),
        ]);
        let audio = json!({"type":"input_audio","input_audio":{"data":"UklGRg==","format":"wav"}});
        let body = json!({"model":"demo","input":[{"role":"user","content":[audio]}]});
        let projected = project_request(&provider, &body, false).expect("portable projection");
        assert_eq!(projected["input"][0]["content"][0], audio);
    }
}
