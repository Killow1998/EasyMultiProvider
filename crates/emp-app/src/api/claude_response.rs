//! Claude error presentation for HTTP and WebSocket callers.
use crate::http::response::{json_error_response, status_text};
use crate::services::claude_cli::{ClaudeCliError, failure_details};
use serde_json::{Value, json};

pub(crate) fn http_response(error: &ClaudeCliError) -> Vec<u8> {
    match error {
        ClaudeCliError::Disconnected => Vec::new(),
        ClaudeCliError::ShuttingDown => json_error_response(
            503,
            status_text(503),
            "EMP is shutting down",
            Some("server_shutting_down"),
            &[],
        ),
        ClaudeCliError::Router(error) => {
            crate::api::failure_response::router_error_response(error.clone())
        }
        ClaudeCliError::Failure(code) => {
            let (status, failure_code, message) = failure_details(code);
            json_error_response(
                status,
                status_text(status),
                message,
                Some(failure_code),
                &[],
            )
        }
    }
}

pub(crate) fn websocket_value(error: &ClaudeCliError) -> Value {
    match error {
        ClaudeCliError::Disconnected => {
            json!({"type":"error","status":499,"error":{"code":"client_disconnected","message":"client disconnected"}})
        }
        ClaudeCliError::ShuttingDown => {
            json!({"type":"error","status":503,"error":{"code":"server_shutting_down","message":"EMP is shutting down"}})
        }
        ClaudeCliError::Router(error) => {
            crate::api::failure_response::websocket_router_error(error)
        }
        ClaudeCliError::Failure(code) => {
            let (status, failure_code, message) = failure_details(code);
            json!({"type":"error","status":status,"error":{"code":failure_code,"message":message}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

            let websocket_value = crate::api::claude_response::websocket_value(&error);
            assert_eq!(websocket_value["status"], expected_status);
            assert_eq!(websocket_value["error"]["code"], expected_code);
        }
    }
}
