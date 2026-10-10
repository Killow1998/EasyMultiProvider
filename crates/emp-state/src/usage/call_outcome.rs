//! Terminal request classification shared by live receipts and durable reports.
use serde_json::Value;

pub fn call_state(event: &Value) -> &'static str {
    if event["error_class"] == "client_cancelled" {
        "interrupted"
    } else if event["error_class"] == "client_disconnect" {
        "cancelled"
    } else if event["error_code"] == "previous_response_not_found" {
        // A request to resend history is neither successful nor a model failure.
        "recovery_required"
    } else if event["success"] == true {
        "completed"
    } else if event["success"] == false
        || event["status"].is_number()
        || matches!(
            event["response_status"].as_str(),
            Some("failed" | "incomplete")
        )
    {
        "failed"
    } else {
        "unknown"
    }
}

// Read projection also corrects pre-fix rows, without rewriting accounting or
// guessing which later request retried a failed attempt. Successful upstream
// accounting remains independent of downstream write failure.
pub(super) const REPORT_CALLS: &str = include_str!("../../../../contracts/call-outcomes.sql");
