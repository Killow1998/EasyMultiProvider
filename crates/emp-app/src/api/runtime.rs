use crate::server::http::error::JsonReply;

/// Resolves a trusted Claude Code CLI path without starting the CLI.
pub(crate) fn claude_cli_availability_request() -> JsonReply {
    let available = emp_codex::installed_cli::resolve_claude_cli().is_some();
    let guidance = if available {
        "Claude Code CLI is available to EMP."
    } else {
        "Claude Code CLI was not found as an available local installation. Install it or check that EMP can access it, then reopen this form."
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "available": available,
        "guidance": guidance,
    }))
    .expect("Claude CLI availability JSON");
    JsonReply::new(200, body)
}
