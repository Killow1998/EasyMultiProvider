//! Bounded destination assessment, separate from CLI process execution.
use super::{ClaudeCliError, media};
use emp_core::ResolvedRoute;
use serde_json::{Value, json};

pub(crate) fn context_estimation_payload(body: &Value) -> Result<Value, ClaudeCliError> {
    media::context_payload(body).map_err(ClaudeCliError::Failure)
}

pub(crate) fn assess_context(
    route: &ResolvedRoute,
    body: &Value,
) -> Result<emp_history::context::ContextAssessment, ClaudeCliError> {
    match context_estimation_payload(body) {
        Ok(payload) => Ok(emp_history::context::assess(
            route.provider.value(),
            route.model.value(),
            route.protocol.as_config_str(),
            &payload,
        )),
        Err(error @ ClaudeCliError::Failure("claude_cli_input_too_large")) => {
            // The reconstructed history may exceed the adapter's encoding budget
            // precisely because it needs compaction. Do not reject it before the
            // history planner can pack bounded summaries. Keep the estimate
            // unknown rather than treating the partial projection as complete.
            let assessment = emp_history::context::assess_with_input_estimate(
                route.provider.value(),
                route.model.value(),
                route.protocol.as_config_str(),
                &json!({"max_tokens":body.get("max_output_tokens")}),
                None,
            );
            if assessment.safe_input_limit.is_some_and(|limit| limit > 0) {
                Ok(assessment)
            } else {
                Err(error)
            }
        }
        Err(error) => Err(error),
    }
}
