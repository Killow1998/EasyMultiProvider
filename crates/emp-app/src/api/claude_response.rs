//! Claude error presentation for HTTP and WebSocket callers.
use crate::http::response::{json_error_response, status_text};
use crate::services::claude_cli::failure_stage;
use crate::services::claude_cli::{ClaudeCliError, failure_details};
use crate::services::failure_feedback;
use emp_core::ResolvedRoute;
use serde_json::{Value, json};

pub(crate) fn http_response(error: &ClaudeCliError) -> Vec<u8> {
    http_response_with_route(error, None)
}

pub(crate) fn http_response_for_route(error: &ClaudeCliError, route: &ResolvedRoute) -> Vec<u8> {
    http_response_with_route(error, Some(route))
}

fn http_response_with_route(error: &ClaudeCliError, route: Option<&ResolvedRoute>) -> Vec<u8> {
    match error {
        ClaudeCliError::Disconnected => Vec::new(),
        ClaudeCliError::ShuttingDown => json_error_response(
            503,
            status_text(503),
            "EMP is shutting down",
            Some("server_shutting_down"),
            &[],
        ),
        ClaudeCliError::Router(error) => route.map_or_else(
            || crate::api::failure_response::router_error_response(error.clone()),
            |route| {
                crate::api::failure_response::router_error_response_for_route(error.clone(), route)
            },
        ),
        ClaudeCliError::Failure(code) => {
            let (status, failure_code, message) = failure_details(code);
            let message = route.map_or_else(
                || message.to_owned(),
                |route| failure_feedback::claude_message(route, message, failure_stage(error)),
            );
            json_error_response(
                status,
                status_text(status),
                &message,
                Some(failure_code),
                &[],
            )
        }
    }
}

pub(crate) fn websocket_value_for_route(error: &ClaudeCliError, route: &ResolvedRoute) -> Value {
    websocket_value_with_route(error, Some(route))
}

fn websocket_value_with_route(error: &ClaudeCliError, route: Option<&ResolvedRoute>) -> Value {
    match error {
        ClaudeCliError::Disconnected => {
            json!({"type":"error","status":499,"error":{"code":"client_disconnected","message":"client disconnected"}})
        }
        ClaudeCliError::ShuttingDown => {
            json!({"type":"error","status":503,"error":{"code":"server_shutting_down","message":"EMP is shutting down"}})
        }
        ClaudeCliError::Router(error) => route.map_or_else(
            || crate::api::failure_response::websocket_router_error(error),
            |route| crate::api::failure_response::websocket_router_error_for_route(error, route),
        ),
        ClaudeCliError::Failure(code) => {
            let (status, failure_code, message) = failure_details(code);
            let message = route.map_or_else(
                || message.to_owned(),
                |route| failure_feedback::claude_message(route, message, failure_stage(error)),
            );
            json!({"type":"error","status":status,"error":{"code":failure_code,"message":message}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use emp_core::{Dialect, Protocol, RouteSource};

    fn route() -> ResolvedRoute {
        let provider = serde_json::json!({
            "id":"claude", "name":"Claude Team", "protocol":"anthropic_messages",
            "auth_mode":"api_key", "base_url":"https://example.invalid/v1"
        })
        .as_object()
        .unwrap()
        .clone();
        let model = serde_json::json!({"id":"claude/5.5", "provider":"claude"})
            .as_object()
            .unwrap()
            .clone();
        ResolvedRoute::new(
            "claude/5.5",
            "claude-sonnet-5-5",
            RouteSource::ExplicitModel,
            provider,
            model,
            Protocol::AnthropicMessages,
            Dialect::PortableResponses,
            "claude",
            format!("sha256:{}", "a".repeat(64)),
            "test-deployment",
        )
        .unwrap()
    }

    #[test]
    fn local_cli_failure_status_and_code_match_http_and_websocket() {
        for (reason, expected_status, expected_code) in [
            (
                "claude_cli_input_too_large",
                413,
                "claude_cli_input_too_large",
            ),
            (
                "claude_cli_schema_too_large",
                413,
                "claude_cli_input_too_large",
            ),
            (
                "claude_cli_invalid_tool_history",
                422,
                "claude_cli_invalid_tool_history",
            ),
        ] {
            let error = ClaudeCliError::Failure(reason);
            let http = String::from_utf8(crate::api::claude_response::http_response(&error))
                .expect("HTTP response UTF-8");
            let (head, body) = http.split_once("\r\n\r\n").expect("HTTP response body");
            assert!(head.starts_with(&format!("HTTP/1.1 {expected_status} ")));
            let http_value: Value = serde_json::from_str(body).expect("HTTP error JSON");
            assert_eq!(http_value["error"]["code"], expected_code);

            let websocket_value = websocket_value_with_route(&error, None);
            assert_eq!(websocket_value["status"], expected_status);
            assert_eq!(websocket_value["error"]["code"], expected_code);
        }
    }

    #[test]
    fn claude_failures_identify_the_selected_cli_model_and_distinct_safe_boundary() {
        let route = route();
        for (reason, status, code, boundary, cause) in [
            (
                "unsupported_reasoning_effort",
                400,
                "unsupported_reasoning_effort",
                "before starting Claude Code CLI",
                "reasoning.effort",
            ),
            (
                "claude_cli_system_format_mismatch",
                502,
                "claude_cli_transcript_mismatch",
                "before EMP forwarded the request",
                "system messages",
            ),
            (
                "claude_cli_content_mismatch",
                502,
                "claude_cli_transcript_mismatch",
                "before EMP forwarded the request",
                "user transcript",
            ),
            (
                "claude_cli_process_failed",
                502,
                "claude_cli_process_failed",
                "while running Claude Code CLI",
                "exited",
            ),
            (
                "claude_cli_result_error",
                502,
                "claude_cli_result_error",
                "while reading the Claude Code result",
                "failed result",
            ),
        ] {
            let error = ClaudeCliError::Failure(reason);
            let http = String::from_utf8(http_response_for_route(&error, &route)).unwrap();
            let (head, body) = http.split_once("\r\n\r\n").unwrap();
            assert!(head.starts_with(&format!("HTTP/1.1 {status} ")));
            let http: Value = serde_json::from_str(body).unwrap();
            let ws = websocket_value_for_route(&error, &route);
            assert_eq!(ws["status"], status);
            assert_eq!(http["error"]["code"], code);
            assert_eq!(ws["error"]["code"], code);
            for value in [&http["error"]["message"], &ws["error"]["message"]] {
                let message = value.as_str().unwrap();
                assert!(message.contains("Claude Team"));
                assert!(message.contains("claude/5.5"));
                assert!(message.contains("claude-sonnet-5-5"));
                assert!(message.contains(boundary));
                assert!(message.contains(cause));
                assert!(!message.contains("private prompt"));
            }
        }
    }
}
