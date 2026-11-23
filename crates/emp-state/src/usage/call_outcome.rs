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

// Correct legacy receipts on read without rewriting accounting or inferring retry relationships;
// successful upstream accounting remains independent of downstream write failure.
pub(super) const REPORT_CALLS: &str = include_str!("../../../../contracts/call-outcomes.sql");
