//! Error provenance comes from the boundary that detected it, not message text.
use crate::services::claude_cli::{ClaudeCliError, failure_stage};
use emp_router::{RouterError, RouterErrorKind};

pub(crate) fn router(error: &RouterError) -> &'static str {
    match error.kind() {
        RouterErrorKind::Upstream => "upstream",
        RouterErrorKind::Transport => "transport",
        _ => "emp",
    }
}

pub(crate) fn claude(error: &ClaudeCliError) -> &'static str {
    match error {
        ClaudeCliError::Router(error) => router(error),
        ClaudeCliError::Disconnected | ClaudeCliError::Failure("claude_cli_interrupted") => {
            "client"
        }
        ClaudeCliError::ShuttingDown => "emp",
        ClaudeCliError::Failure(_) => match failure_stage(error) {
            "input_validation" | "relay_validation" | "relay_request" => "emp",
            // CLI failures can wrap a service rejection; keep the observed boundary.
            _ => "claude_cli",
        },
    }
}

pub(crate) fn message(origin: &str, message: &str) -> String {
    let label = match origin {
        "emp" => "EMP",
        "upstream" => "Upstream service",
        "transport" => "EMP connection",
        "claude_cli" => "Claude Code CLI",
        "client" => "Client",
        _ => "Unknown source",
    };
    format!("[{label}] {message}")
}

pub(crate) fn annotate(detail: &mut serde_json::Value, origin: &str) {
    detail["origin"] = origin.into();
    if let Some(text) = detail["message"].as_str() {
        detail["message"] = message(origin, text).into();
    }
}
