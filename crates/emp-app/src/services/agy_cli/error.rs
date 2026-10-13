//! Stable public failures; CLI stderr and account details never enter responses.
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Error(pub(crate) &'static str);

impl Error {
    pub(crate) fn details(self) -> (u16, &'static str) {
        match self.0 {
            "previous_response_not_found" => (
                400,
                "The agy CLI requires the complete conversation. Resend the full history without previous_response_id.",
            ),
            "agy_cli_model_unavailable" => (
                400,
                "The installed agy CLI does not support this model and effort combination. Update the model list and select an available entry.",
            ),
            "agy_cli_unavailable" => (
                503,
                "Install the official Antigravity CLI (agy) and make it available to EMP.",
            ),
            "agy_cli_home_unavailable" => (503, "EMP cannot access the local agy profile."),
            "agy_cli_customizations_present" => (
                409,
                "This agy profile contains customizations that can execute on the EMP host. Use a clean official CLI profile for client-owned tools.",
            ),
            "agy_cli_login_mode_required" => (
                400,
                "This service requires official agy sign-in, not Gemini API-key mode.",
            ),
            "agy_cli_login_required" => (
                401,
                "Sign in using the official agy CLI under the operating-system account running EMP.",
            ),
            "agy_cli_unsupported_input" => (
                400,
                "The agy headless adapter accepts text and text tool results. This input contains unsupported media.",
            ),
            "agy_cli_unsupported_effort" => (
                400,
                "agy supports low, medium, high, xhigh or max effort; omit effort to use its default.",
            ),
            "agy_cli_unsupported_parameter" => (
                400,
                "The agy CLI cannot enforce generation limits, sampling settings, generate:false or a client output schema. Omit these parameters for this service.",
            ),
            "agy_cli_tool_choice_unavailable" => (
                400,
                "The requested tool is outside the agy read/write/edit/command tool set.",
            ),
            "agy_cli_input_too_large" => (
                413,
                "The agy transcript exceeds the adapter's 10 MiB limit.",
            ),
            "agy_cli_invalid_input" => (422, "Invalid agy transcript or client tool definitions."),
            "agy_cli_timeout" => (504, "The official agy CLI request timed out."),
            "client_disconnected" | "agy_cli_interrupted" => {
                (499, "The client cancelled the agy request.")
            }
            "server_shutting_down" => (503, "EMP is shutting down."),
            "agy_cli_unknown_tool_proposal" => (
                502,
                "agy proposed a tool that is not available in this request.",
            ),
            "agy_cli_local_tool_attempt" => (
                502,
                "agy attempted an unexpected local tool; EMP stopped this invocation.",
            ),
            "agy_cli_invalid_output" => (
                502,
                "agy returned an invalid or incomplete structured result.",
            ),
            "agy_cli_output_too_large" => (502, "agy exceeded the bounded output limit."),
            _ => (
                502,
                "The official agy CLI could not complete this request. Check CLI sign-in and model access.",
            ),
        }
    }

    pub(crate) fn origin(self) -> &'static str {
        if matches!(self.0, "client_disconnected" | "agy_cli_interrupted") {
            "client"
        } else if self.details().0 < 500 || self.0 == "server_shutting_down" {
            "emp"
        } else {
            "agy_cli"
        }
    }

    pub(crate) fn event(self) -> Value {
        let (status, message) = self.details();
        json!({"type":"error","status":status,"error":{
            "code":self.0,"origin":self.origin(),
            "message":crate::services::error_origin::message(self.origin(),message)
        }})
    }
}
