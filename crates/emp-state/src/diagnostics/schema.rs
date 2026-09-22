//! Typed allowlist matching Python ObservationRing; raw content never passes.
use super::Journal;
use regex::Regex;
use serde_json::{Value, json};
use std::sync::LazyLock;
static ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._/:-]{1,256}$").unwrap());
static MODEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:/@+~-]{0,255}$").unwrap());
static FINGERPRINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^sha256:[0-9a-f]{64}$").unwrap());
static STAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d{1,6})?(?:Z|[+-]\d{2}:\d{2})$").unwrap()
});
static UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});
fn text(value: &Value, pattern: &Regex) -> String {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| pattern.is_match(text))
        .unwrap_or("")
        .into()
}
pub fn id(value: &Value) -> String {
    text(value, &ID)
}
pub fn integer(value: &Value, maximum: i64) -> Value {
    let number = match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().filter(|v| v.is_finite()).map(|v| v as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    };
    json!(number.map(|value| value.clamp(0, maximum)))
}
pub(super) fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
    .filter(|value| value.is_finite())
}
fn select(value: &Value, choices: &[&str], default: &str) -> String {
    value
        .as_str()
        .filter(|value| choices.contains(value))
        .unwrap_or(default)
        .into()
}
fn fallback<'a>(event: &'a Value, key: &str, alternative: &str) -> &'a Value {
    event.get(key).unwrap_or(&event[alternative])
}
fn truth(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
pub fn route_record(event: &Value, journal: &Journal) -> Value {
    let mut record = serde_json::Map::new();
    for (target, source, choices, default) in [
        (
            "protocol",
            "resolved_protocol",
            &[
                "responses",
                "chat_completions",
                "anthropic_messages",
                "unknown",
            ][..],
            "unknown",
        ),
        (
            "dialect",
            "dialect",
            &[
                "codex_native",
                "portable_responses",
                "chat_completions",
                "anthropic_messages",
                "unknown",
            ],
            "unknown",
        ),
        (
            "transport",
            "transport",
            &["http", "sse", "websocket", "unknown"],
            "unknown",
        ),
        (
            "stream_phase",
            "phase",
            &[
                "connect",
                "first_event",
                "streaming",
                "terminal_validation",
                "unknown",
            ],
            "unknown",
        ),
        (
            "speed_mode",
            "speed_mode",
            &["standard", "fast", "unknown"],
            "unknown",
        ),
        (
            "decision",
            "protocol_decision",
            &[
                "explicit",
                "normal_order",
                "observed_priority",
                "fallback_rejection",
                "unknown",
            ],
            "unknown",
        ),
        (
            "tool_pairing_status",
            "tool_pairing_status",
            &["none", "standalone", "paired", "incomplete", "invalid"],
            "invalid",
        ),
        (
            "recovery_mode",
            "recovery_mode",
            &[
                "none",
                "pre_output_retry",
                "reattached",
                "native_http_fallback",
                "previous_response_not_found",
            ],
            "none",
        ),
        (
            "route_source",
            "route_source",
            &[
                "explicit_model",
                "subscription_account",
                "forward_provider",
                "implicit_native",
                "unknown",
            ],
            "unknown",
        ),
        (
            "fallback_reason",
            "fallback_reason",
            &[
                "none",
                "protocol_rejection",
                "native_websocket_rejection",
                "unknown",
            ],
            "unknown",
        ),
        (
            "response_model_source",
            "response_model_source",
            &["upstream_response", "missing"],
            "missing",
        ),
        (
            "upstream_content_encoding",
            "upstream_content_encoding",
            &["zstd", "identity"],
            "unknown",
        ),
    ] {
        let value = if matches!(target, "protocol" | "decision") {
            fallback(event, source, target)
        } else {
            &event[source]
        };
        let default = if event.get(source).is_none()
            && matches!(target, "tool_pairing_status" | "fallback_reason")
        {
            "none"
        } else {
            default
        };
        record.insert(target.into(), json!(select(value, choices, default)));
    }
    let error = select(
        &event["error_class"],
        &[
            "none",
            "auth",
            "payment_required",
            "rate_limit",
            "protocol_rejection",
            "upstream_5xx",
            "upstream_504",
            "timeout",
            "connect_timeout",
            "first_event_timeout",
            "first_output_timeout",
            "idle_after_output",
            "local_deadline",
            "network",
            "proxy_unavailable",
            "dns_failure",
            "tls_failure",
            "router_error",
            "stream_error",
            "stream_incomplete",
            "client_disconnect",
            "client_cancelled",
            "client_websocket_close",
            "upstream_close_pre_output",
            "upstream_close_after_output",
            "upstream_close_after_tool",
            "upstream_capacity",
            "malformed_terminal",
            "proxy_reset",
            "output_limit",
            "content_filter",
            "context_length_exceeded",
            "external_compaction_failed",
            "history_reconstruction_failed",
            "unknown",
        ],
        "unknown",
    );
    record.insert("error_class".into(), json!(error));
    for key in ["provider_id", "model_id", "failure_reason"] {
        record.insert(key.into(), json!(id(&event[key])));
    }
    for key in ["client_model", "upstream_model", "response_model"] {
        let value = text(&event[key], &MODEL);
        record.insert(
            key.into(),
            json!(if value.is_empty() { "unknown" } else { &value }),
        );
    }
    let route = id(&event["route"]);
    record.insert(
        "route".into(),
        json!(if route.is_empty() { "unknown" } else { &route }),
    );
    record.insert(
        "endpoint_fingerprint".into(),
        json!(text(&event["endpoint_fingerprint"], &FINGERPRINT)),
    );
    let deployment = id(&event["deployment_identity"]);
    record.insert(
        "deployment_identity".into(),
        json!(if deployment.is_empty() {
            "default"
        } else {
            &deployment
        }),
    );
    let identity = id(&event["observation_id"]);
    record.insert(
        "observation_id".into(),
        json!(if identity.is_empty() {
            super::random_id(16)
        } else {
            identity
        }),
    );
    let stamp = event["observed_at"]
        .as_str()
        .filter(|stamp| STAMP.is_match(stamp))
        .map(str::to_owned)
        .unwrap_or_else(|| super::timestamp().replace('Z', "+00:00"));
    record.insert("observed_at".into(), json!(stamp));
    for (field, max) in [
        ("local_prepare_ms", 60_000),
        ("upstream_first_event_ms", 3_600_000),
        ("ttft_ms", 3_600_000),
        ("upstream_first_token_ms", 3_600_000),
        ("generation_ms", 3_600_000),
        ("output_tokens", 10_000_000),
        ("request_item_count", 256),
        ("performance_schema", 100),
        ("request_bytes", 64 * 1024 * 1024),
        ("decoded_request_bytes", 64 * 1024 * 1024),
        ("upstream_request_bytes", 64 * 1024 * 1024),
        ("response_bytes", 64 * 1024 * 1024),
    ] {
        record.insert(field.into(), integer(&event[field], max));
    }
    for field in [
        "output_emitted",
        "tool_activity",
        "terminal_event_observed",
        "recovery_succeeded",
        "connection_reused",
    ] {
        record.insert(field.into(), json!(truth(&event[field])));
    }
    record.insert(
        "fallback".into(),
        json!(truth(fallback(event, "protocol_fallback", "fallback"))),
    );
    record.insert(
        "retry_count".into(),
        json!(integer(&event["retry_count"], 10).as_i64().unwrap_or(0)),
    );
    record.insert(
        "duration_ms".into(),
        json!(
            number(&event["duration_ms"])
                .unwrap_or(0.0)
                .round_ties_even()
                .clamp(0.0, 3_600_000.0) as i64
        ),
    );
    record.insert(
        "status".into(),
        json!(
            integer(&event["status"], 599)
                .as_i64()
                .filter(|status| *status >= 100)
        ),
    );
    record.insert(
        "close_code".into(),
        json!(
            integer(&event["close_code"], 4999)
                .as_i64()
                .filter(|code| *code >= 1000)
        ),
    );
    let input = crate::usage::token_count(event.get("input_tokens"));
    let cached = crate::usage::token_count(event.get("cached_input_tokens"))
        .filter(|cached| input.is_some_and(|input| *cached <= input));
    record.insert("input_tokens".into(), json!(input));
    record.insert("cached_input_tokens".into(), json!(cached));
    record.insert(
        "tokens_per_second".into(),
        json!(
            number(&event["tokens_per_second"])
                .map(|value| (value.clamp(0.0, 100_000.0) * 100.0).round_ties_even() / 100.0)
        ),
    );
    if record["decoded_request_bytes"] != json!(0)
        && let Some(ratio) =
            number(&event["compression_ratio"]).filter(|value| (0.0..=64.0).contains(value))
    {
        record.insert("compression_ratio".into(), json!(ratio));
    }
    for field in ["request_item_types", "content_part_types"] {
        record.insert(
            field.into(),
            json!(
                event[field]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(256)
                    .map(id)
                    .filter(|value| !value.is_empty())
                    .collect::<Vec<_>>()
            ),
        );
    }
    record.insert(
        "model_trace_source".into(),
        json!(if event["model_trace_source"] == "emp_dispatch" {
            "emp_dispatch"
        } else {
            "unknown"
        }),
    );
    let context = &event["context_observation"];
    let decision = match fallback(context, "context_decision", "decision").as_str() {
        Some("allow" | "allowed") => "allowed",
        Some("warn" | "warned") => "warned",
        Some("block" | "blocked") => "blocked",
        _ => "unknown",
    };
    record.insert("context_decision".into(), json!(decision));
    record.insert(
        "context_source".into(),
        json!(select(
            &context["source"],
            &[
                "official",
                "advertised",
                "observed",
                "manual",
                "inherited",
                "inferred",
                "unknown"
            ],
            "unknown"
        )),
    );
    record.insert(
        "context_completeness".into(),
        json!(select(
            &context["completeness"],
            &["high", "lost", "unknown"],
            "unknown"
        )),
    );
    record.insert(
        "context_confidence".into(),
        json!(
            number(&context["confidence"])
                .unwrap_or(if context["confidence"] == true {
                    1.0
                } else {
                    0.0
                })
                .clamp(0.0, 1.0)
        ),
    );
    for (target, source) in [
        ("context_limit", "context_limit"),
        ("safe_input_limit", "safe_input_limit"),
        ("context_reserves", "reserves"),
    ] {
        record.insert(target.into(), integer(&context[source], 100_000_000));
    }
    record.insert(
        "estimated_tokens".into(),
        integer(
            fallback(context, "estimated_tokens", "input_estimate"),
            100_000_000,
        ),
    );
    record.insert(
        "context_estimate_method".into(),
        json!(id(&context["estimate_method"])),
    );
    let mut claims = false;
    for (raw, reference) in [
        ("thread_id", "thread_ref"),
        ("session_id", "session_ref"),
        ("turn_id", "turn_ref"),
        ("parent_thread_id", "parent_thread_ref"),
    ] {
        if let Some(value) = event[raw].as_str().filter(|value| UUID.is_match(value)) {
            record.insert(
                reference.into(),
                json!(journal.pseudonym(&value.to_ascii_lowercase())),
            );
            claims = true;
        }
        let retained = id(&event[reference]);
        if !retained.is_empty() {
            record.insert(reference.into(), json!(retained));
        }
    }
    let request = event["request_id"].as_str().filter(|value| {
        (16..=32).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if let Some(request) = request {
        record.insert("request_id".into(), json!(request));
    }
    let client = select(
        &event["client_kind"],
        &["codex_cli_rs", "codex_vscode", "codex_desktop", "codex_app"],
        "unknown",
    );
    record.insert(
        "client_kind_source".into(),
        json!(if client == "unknown" {
            "unknown"
        } else {
            "originator_header"
        }),
    );
    record.insert("client_kind".into(), json!(client));
    record.insert(
        "identity_source".into(),
        json!(if claims { "client_claim" } else { "unknown" }),
    );
    record.insert("call_purpose".into(), json!("unknown"));
    record.insert("call_purpose_source".into(), json!("not_provided"));
    Value::Object(record)
}
