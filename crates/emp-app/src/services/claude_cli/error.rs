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
            "Claude Code could not complete this local subscription request; check selected model access and local sign-in, then inspect EMP diagnostics",
        ),
        "claude_cli_timeout" => (
            504,
            "claude_cli_timeout",
            "Claude Code CLI request timed out",
        ),
        "claude_cli_interrupted" => (499, "claude_cli_interrupted", "Claude request interrupted"),
        "claude_cli_output_budget_exhausted" => (
            502,
            "claude_cli_output_budget_exhausted",
            "Claude reached the output token limit before completing its response. Increase the model output limit or max_output_tokens and retry.",
        ),
        "claude_cli_models_unsupported" => (
            502,
            "claude_cli_models_unsupported",
            "Claude Code returned no model list; update Claude Code and retry",
        ),
        "claude_cli_invalid_model_list" | "claude_cli_model_list_too_large" => (
            502,
            code,
            "Claude Code returned an invalid model list; retry updating the list",
        ),
        "unsupported_reasoning_effort_none" => (
            400,
            "unsupported_reasoning_effort",
            "EMP received reasoning.effort=\"none\"; the Claude Code CLI adapter cannot safely disable thinking for this route. Select a supported effort level offered for this model and retry",
        ),
        "unsupported_reasoning_effort" => (
            400,
            "unsupported_reasoning_effort",
            "EMP's Claude Code CLI adapter does not support this reasoning.effort; use low, medium, high, xhigh, max, or omit the optional value",
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
            "EMP could not verify the Claude Code request transcript; check the selected model and retry, then inspect EMP diagnostics",
        ),
        "claude_cli_system_format_mismatch" => (
            502,
            "claude_cli_transcript_mismatch",
            "EMP could not safely normalize Claude Code system messages; check CLI compatibility and inspect EMP diagnostics",
        ),
        "claude_cli_content_mismatch" => (
            502,
            "claude_cli_transcript_mismatch",
            "EMP could not verify that Claude Code preserved the user transcript; check CLI compatibility and inspect EMP diagnostics",
        ),
        "claude_cli_process_failed" => (
            502,
            "claude_cli_process_failed",
            "Claude Code CLI exited without a successful result; check the configured model and Claude Code access, then inspect EMP diagnostics",
        ),
        "claude_cli_stdin_failed" => (
            502,
            "claude_cli_stdin_failed",
            "Claude Code CLI closed its input before EMP finished sending the request; check the configured model and inspect EMP diagnostics",
        ),
        "claude_cli_output_too_large" => (
            502,
            "claude_cli_output_too_large",
            "Claude Code CLI exceeded EMP's bounded output limit; inspect EMP diagnostics",
        ),
        "claude_cli_result_error" => (
            502,
            "claude_cli_result_error",
            "Claude Code CLI returned a failed result; check model access and inspect EMP diagnostics",
        ),
        "claude_cli_missing_structured_output" => (
            502,
            "claude_cli_missing_structured_output",
            "Claude Code CLI returned no structured result for this model; check CLI compatibility and inspect EMP diagnostics",
        ),
        "claude_cli_invalid_output" => (
            502,
            "claude_cli_invalid_output",
            "Claude Code CLI output could not be parsed; check CLI compatibility and inspect EMP diagnostics",
        ),
        "claude_cli_no_provider_response" => (
            502,
            "claude_cli_no_provider_response",
            "Claude Code CLI did not send an inference request through EMP's relay; check CLI compatibility and inspect EMP diagnostics",
        ),
        _ => (502, code, "Claude Code CLI request failed"),
    }
}

pub(crate) fn failure_stage(error: &ClaudeCliError) -> &'static str {
    match error {
        ClaudeCliError::Disconnected
        | ClaudeCliError::ShuttingDown
        | ClaudeCliError::Failure("claude_cli_interrupted") => "cancelled",
        ClaudeCliError::Router(_) => "upstream_request",
        ClaudeCliError::Failure(
            "unsupported_reasoning_effort" | "unsupported_reasoning_effort_none",
        ) => "input_validation",
        ClaudeCliError::Failure(code) if code.starts_with("claude_cli_relay_") => "relay_request",
        ClaudeCliError::Failure(
            "claude_cli_system_format_mismatch"
            | "claude_cli_content_mismatch"
            | "claude_cli_transcript_mismatch"
            | "claude_cli_tools_not_disabled",
        ) => "relay_validation",
        ClaudeCliError::Failure(
            "claude_cli_process_failed" | "claude_cli_spawn_failed" | "claude_cli_stdin_failed",
        ) => "cli_process",
        ClaudeCliError::Failure("claude_cli_local_request_failed") => "cli_execution",
        ClaudeCliError::Failure("claude_cli_output_too_large") => "cli_output",
        ClaudeCliError::Failure("claude_cli_output_budget_exhausted") => "output_budget",
        ClaudeCliError::Failure(
            "claude_cli_result_error"
            | "claude_cli_missing_structured_output"
            | "claude_cli_invalid_output",
        ) => "cli_result",
        ClaudeCliError::Failure("claude_cli_timeout") => "cli_timeout",
        ClaudeCliError::Failure(_) => "unknown",
    }
}
