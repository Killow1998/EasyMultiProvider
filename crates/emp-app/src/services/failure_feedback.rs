//! Public explanation for a failed request using its immutable selected route.

use emp_core::ResolvedRoute;
use emp_router::native_http::NativeHttpError;
use emp_transport::{
    FailureClass, normalize_error_class, public_failure_message, status_error_class,
};

fn label(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(96)
        .collect()
}

pub(crate) fn message(
    route: &ResolvedRoute,
    class: FailureClass,
    reason: Option<&str>,
    status: u16,
    output_started: bool,
    local_detail: Option<&str>,
) -> String {
    let source = route
        .provider
        .value()
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(&route.provider_id);
    let cause = local_detail
        .map(label)
        .unwrap_or_else(|| public_failure_message(class, reason, status));
    let progress = if output_started {
        "Some output was delivered, but this turn did not complete."
    } else {
        "No output was delivered; upstream execution and billing are unknown."
    };
    let action = match (class, reason) {
        (FailureClass::RateLimit, Some("quota_exhausted")) => {
            "Check this account's quota, wait for recovery, or manually choose another source. Recovery time is unknown unless the upstream provides it."
        }
        (FailureClass::RateLimit | FailureClass::UpstreamCapacity, _) => {
            "Wait and retry, or manually choose another source."
        }
        (FailureClass::Auth, _) => "Check this source's credentials in EMP, then retry.",
        (FailureClass::ContextLengthExceeded, _) => {
            "Reduce the input or choose a model with a larger context window."
        }
        (
            FailureClass::Network
            | FailureClass::DnsFailure
            | FailureClass::TlsFailure
            | FailureClass::ProxyUnavailable
            | FailureClass::ProxyReset
            | FailureClass::ConnectTimeout
            | FailureClass::FirstEventTimeout
            | FailureClass::FirstOutputTimeout
            | FailureClass::IdleAfterOutput
            | FailureClass::Timeout
            | FailureClass::LocalDeadline,
            _,
        ) => "Check the connection, then retry if appropriate.",
        _ => "Inspect EMP diagnostics, then retry or manually choose another source.",
    };
    format!(
        "Selected source '{}' and model '{}': {cause} {progress} {action}",
        label(source),
        label(&route.requested_model),
    )
}

pub(crate) fn annotate_native(error: &mut NativeHttpError, route: &ResolvedRoute) {
    let Some(reason) = error.body["error"]["failure_reason"].as_str() else {
        // A local preparation or credential error has no upstream evidence.
        return;
    };
    let class = normalize_error_class(
        error.body["error"]["type"].as_str(),
        status_error_class(Some(error.status)),
    );
    error.body["error"]["message"] = serde_json::Value::String(message(
        route,
        class,
        Some(reason),
        error.status,
        false,
        None,
    ));
}

pub(crate) fn claude_message(route: &ResolvedRoute, cause: &str, stage: &str) -> String {
    let source = route
        .provider
        .value()
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(&route.provider_id);
    let (boundary, progress) = match stage {
        "relay_validation" | "relay_request" => (
            "before EMP forwarded the request",
            "No assistant output was delivered.",
        ),
        "cli_process" => (
            "while running Claude Code CLI",
            "No assistant output was delivered; upstream execution and billing are unknown.",
        ),
        "cli_result" | "cli_output" => (
            "while reading the Claude Code result",
            "No assistant output was delivered; upstream execution and billing are unknown.",
        ),
        "upstream_request" => (
            "while contacting the selected service",
            "No assistant output was delivered; upstream execution and billing are unknown.",
        ),
        _ => (
            "at an unknown stage",
            "No assistant output was delivered; upstream execution and billing are unknown.",
        ),
    };
    format!(
        "Selected source '{}' and model '{}' (CLI model '{}'): {cause}. Failure occurred {boundary}. {progress}",
        label(source),
        label(&route.requested_model),
        label(&route.upstream_model),
    )
}
