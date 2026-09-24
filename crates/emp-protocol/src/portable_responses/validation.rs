//! Terminal and output-item validation for portable Responses.

use super::*;

fn validation_error(
    kind: ResponsesValidationErrorKind,
    message: &'static str,
) -> ResponsesValidationError {
    ResponsesValidationError::new(kind, message)
}

fn required_response_string(
    item: &Map<String, Value>,
    field: &str,
    kind: ResponsesValidationErrorKind,
    message: &'static str,
) -> Result<(), ResponsesValidationError> {
    if item
        .get(field)
        .and_then(Value::as_str)
        .is_some_and(|value| !value.is_empty())
    {
        Ok(())
    } else {
        Err(validation_error(kind, message))
    }
}

fn validate_output_item(item: &Map<String, Value>) -> Result<(), ResponsesValidationError> {
    const INVALID_ITEM: &str = "upstream Responses JSON contains an invalid output item";
    let Some(kind) = item.get("type").and_then(Value::as_str) else {
        return Err(validation_error(
            ResponsesValidationErrorKind::UnsupportedOutputItem,
            "upstream Responses JSON contains an unsupported output item",
        ));
    };
    if !matches!(
        kind,
        "message"
            | "function_call"
            | "custom_tool_call"
            | "tool_search_call"
            | "reasoning"
            | "compaction"
    ) {
        return Err(validation_error(
            ResponsesValidationErrorKind::UnsupportedOutputItem,
            "upstream Responses JSON contains an unsupported output item",
        ));
    }
    match kind {
        "message" => {
            let Some(content) = item.get("content").and_then(Value::as_array) else {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidMessage,
                    "upstream Responses JSON contains an invalid message item",
                ));
            };
            if item.get("role").and_then(Value::as_str) != Some("assistant") {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidMessage,
                    "upstream Responses JSON contains an invalid message item",
                ));
            }
            for raw in content {
                let Some(part) = object(raw) else {
                    return Err(validation_error(
                        ResponsesValidationErrorKind::InvalidMessageContent,
                        "upstream Responses JSON contains invalid message content",
                    ));
                };
                match part.get("type").and_then(Value::as_str) {
                    Some("output_text")
                        if part.get("text").is_some_and(Value::is_string)
                            && matches!(
                                part.get("annotations"),
                                None | Some(Value::Null | Value::Array(_))
                            ) => {}
                    Some("refusal") if part.get("refusal").is_some_and(Value::is_string) => {}
                    Some("output_text" | "refusal") => {
                        return Err(validation_error(
                            ResponsesValidationErrorKind::InvalidMessageContent,
                            "upstream Responses JSON contains invalid message content",
                        ));
                    }
                    _ => {
                        return Err(validation_error(
                            ResponsesValidationErrorKind::UnsupportedMessageContent,
                            "upstream Responses JSON contains unsupported message content",
                        ));
                    }
                }
            }
        }
        "function_call" => {
            required_response_string(
                item,
                "call_id",
                ResponsesValidationErrorKind::InvalidOutputItem,
                INVALID_ITEM,
            )?;
            required_response_string(
                item,
                "name",
                ResponsesValidationErrorKind::InvalidOutputItem,
                INVALID_ITEM,
            )?;
            let valid_arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|value| serde_json::from_str::<Value>(value).ok())
                .is_some_and(|value| value.is_object());
            if !valid_arguments {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidToolCall,
                    "Responses upstream returned invalid tool arguments",
                ));
            }
        }
        "custom_tool_call" => {
            for field in ["call_id", "name", "input"] {
                required_response_string(
                    item,
                    field,
                    ResponsesValidationErrorKind::InvalidOutputItem,
                    INVALID_ITEM,
                )?;
            }
        }
        "tool_search_call" => {
            required_response_string(
                item,
                "call_id",
                ResponsesValidationErrorKind::InvalidOutputItem,
                INVALID_ITEM,
            )?;
            if item.get("execution").and_then(Value::as_str) != Some("client")
                || !item.get("arguments").is_some_and(Value::is_object)
            {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidToolSearch,
                    "upstream Responses JSON contains invalid tool search",
                ));
            }
        }
        "reasoning" => {
            let valid_summary = match item.get("summary") {
                None | Some(Value::Null) => true,
                Some(Value::Array(parts)) => parts.iter().all(|raw| {
                    object(raw).is_some_and(|part| {
                        part.get("type").and_then(Value::as_str) == Some("summary_text")
                            && part.get("text").is_some_and(Value::is_string)
                    })
                }),
                Some(_) => false,
            };
            let valid_opaque = matches!(
                item.get("encrypted_content"),
                None | Some(Value::Null | Value::String(_))
            );
            if !valid_summary || !valid_opaque {
                return Err(validation_error(
                    ResponsesValidationErrorKind::InvalidReasoning,
                    "upstream Responses JSON contains invalid reasoning output",
                ));
            }
        }
        "compaction" => {
            required_response_string(
                item,
                "encrypted_content",
                ResponsesValidationErrorKind::InvalidOpaqueOutput,
                INVALID_ITEM,
            )?;
        }
        _ => unreachable!("output kind was checked above"),
    }
    Ok(())
}

pub fn validate_responses_body(
    value: &Value,
    validate_output_items: bool,
) -> Result<(), ResponsesValidationError> {
    let Some(response) = object(value) else {
        return Err(validation_error(
            ResponsesValidationErrorKind::NotObject,
            "upstream Responses JSON is not an object",
        ));
    };
    let status = response
        .get("status")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            validation_error(
                ResponsesValidationErrorKind::MissingStatus,
                "upstream Responses JSON has no response status",
            )
        })?;
    if !matches!(status, "completed" | "incomplete" | "failed") {
        return Err(validation_error(
            ResponsesValidationErrorKind::UnknownStatus,
            "upstream Responses JSON has an unknown response status",
        ));
    }
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            validation_error(
                ResponsesValidationErrorKind::InvalidOutput,
                "upstream Responses JSON has no valid output",
            )
        })?;
    for raw in output {
        let Some(item) = object(raw) else {
            return Err(validation_error(
                ResponsesValidationErrorKind::InvalidOutputItem,
                "upstream Responses JSON contains an invalid output item",
            ));
        };
        if validate_output_items {
            validate_output_item(item)?;
        }
    }
    if response.contains_key("output_text")
        && !response.get("output_text").is_some_and(Value::is_string)
    {
        return Err(validation_error(
            ResponsesValidationErrorKind::InvalidOutputText,
            "upstream Responses JSON has invalid output text",
        ));
    }
    let upstream_error = response.get("error");
    if status == "failed" {
        if upstream_error
            .and_then(Value::as_object)
            .is_none_or(Map::is_empty)
        {
            return Err(validation_error(
                ResponsesValidationErrorKind::MissingFailure,
                "upstream Responses JSON has a failed status without an error",
            ));
        }
    } else if !matches!(upstream_error, None | Some(Value::Null))
        && !upstream_error.is_some_and(|value| value.as_object().is_some_and(Map::is_empty))
    {
        return Err(validation_error(
            ResponsesValidationErrorKind::ContradictoryFailure,
            "upstream Responses JSON has a contradictory error",
        ));
    }
    let incomplete = response.get("incomplete_details");
    if status == "incomplete" {
        if !matches!(incomplete, None | Some(Value::Null | Value::Object(_))) {
            return Err(validation_error(
                ResponsesValidationErrorKind::InvalidIncompleteDetails,
                "upstream Responses JSON has invalid incomplete details",
            ));
        }
    } else if !matches!(incomplete, None | Some(Value::Null))
        && !incomplete.is_some_and(|value| value.as_object().is_some_and(Map::is_empty))
    {
        return Err(validation_error(
            ResponsesValidationErrorKind::ContradictoryIncompleteDetails,
            "upstream Responses JSON has contradictory incomplete details",
        ));
    }
    Ok(())
}

pub fn terminal_observation(
    value: &Value,
    validate_output_items: bool,
) -> Result<TerminalObservation, ResponsesValidationError> {
    validate_responses_body(value, validate_output_items)?;
    let response = value.as_object().expect("validated Responses object");
    Ok(
        match response["status"].as_str().expect("validated status") {
            "completed" => TerminalObservation {
                status: 200,
                success: true,
                error_class: "none",
            },
            "incomplete" => {
                let reason = response
                    .get("incomplete_details")
                    .and_then(Value::as_object)
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                TerminalObservation {
                    status: 200,
                    success: false,
                    error_class: match reason {
                        "max_output_tokens" => "output_limit",
                        "content_filter" => "content_filter",
                        _ => "stream_incomplete",
                    },
                }
            }
            "failed" => TerminalObservation {
                status: 502,
                success: false,
                error_class: "stream_error",
            },
            _ => unreachable!("validated terminal status"),
        },
    )
}
