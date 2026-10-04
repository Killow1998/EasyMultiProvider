//! Claude execution failures and public classification, independent of delivery.
use emp_router::RouterError;

#[derive(Debug)]
pub(crate) enum ClaudeCliError {
    Disconnected,
    ShuttingDown,
    Router(RouterError),
    Failure(&'static str),
}

pub(crate) fn failure_details(code: &'static str) -> (u16, &'static str, &'static str) {
    match code {
        "claude_cli_unavailable" => (
            503,
            "claude_cli_unavailable",
            "Claude Code CLI is unavailable or not trusted",
        ),
        "claude_cli_local_home_unavailable" => (
            503,
            "claude_cli_local_home_unavailable",
            "Claude Code local sign-in is unavailable to EMP; sign in using the same operating-system account that runs EMP",
        ),
        "claude_cli_login_required" => (
            401,
            "claude_cli_login_required",
            "Claude Code is not signed in to a Claude subscription; sign in to Claude Code manually and retry",
        ),
        "claude_cli_subscription_required" => (
            403,
            "claude_cli_subscription_required",
            "The current Claude Code login is not a Claude subscription login; sign in to the intended Claude subscription and retry",
        ),
        "claude_cli_auth_status_unknown" | "claude_cli_auth_status_unavailable" => (
            503,
            "claude_cli_auth_status_unavailable",
            "EMP could not verify Claude Code subscription sign-in; check Claude Code sign-in and retry",
        ),
        "claude_cli_auth_status_timeout" => (
            504,
            "claude_cli_auth_status_timeout",
            "Claude Code sign-in status check timed out; try again",
        ),
        "claude_cli_local_credentials_present" => (
            400,
            "claude_cli_local_credentials_present",
            "Claude Code local login cannot include provider URL or API key credentials",
        ),
        "claude_cli_local_request_failed" => (
            502,
            "claude_cli_local_request_failed",
            "Claude Code could not complete this local subscription request; check Claude Code sign-in and retry",
        ),
        "claude_cli_timeout" => (
            504,
            "claude_cli_timeout",
            "Claude Code CLI request timed out",
        ),
        "unsupported_reasoning_effort" => (
            400,
            "unsupported_reasoning_effort",
            "reasoning effort is not supported by Claude Code CLI",
        ),
        "unsupported_input_modality" => (
            400,
            "unsupported_input_modality",
            "Claude Code CLI cannot safely resolve this image, audio, video, or document input",
        ),
        "claude_cli_input_too_large" | "claude_cli_schema_too_large" => (
            413,
            "claude_cli_input_too_large",
            "Claude Code CLI request exceeds its size limit",
        ),
        "unsupported_provider_protocol" => (
            501,
            "unsupported_provider_protocol",
            "Claude Code CLI requires an Anthropic Messages provider route",
        ),
        "claude_cli_invalid_tool_history" => (
            422,
            "claude_cli_invalid_tool_history",
            "Responses tool history is invalid for this request",
        ),
        "claude_cli_unknown_tool_proposal" => (
            502,
            "claude_cli_unknown_tool_proposal",
            "Claude Code proposed a tool that is not available in this request",
        ),
        "claude_cli_transcript_mismatch" => (
            502,
            "claude_cli_transcript_mismatch",
            "Claude Code changed the conversation format; EMP could not forward the request",
        ),
        _ => (502, code, "Claude Code CLI request failed"),
    }
}
