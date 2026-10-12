//! Codex quota RPC errors and allowlisted public response projection.
use super::projection::{mask_email, read_limits, safe_credit_snapshot};
use super::*;

/// Classify one JSON-RPC error without exposing its upstream URL or body.
pub fn quota_rpc_error(method: &str, error: &Value) -> QuotaError {
    let mut result = classify_rpc_error(method, error);
    result.rpc_method = match method {
        "account/read" => Some("account/read"),
        "account/rateLimits/read" => Some("account/rateLimits/read"),
        "account/rateLimitResetCredit/consume" => Some("account/rateLimitResetCredit/consume"),
        "initialize" => Some("initialize"),
        _ => None,
    };
    let message = error["message"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    result.http_status = rpc_http_status(&message);
    result.transport_cause = if message.contains("dns") || message.contains("name resolution") {
        Some("dns")
    } else if message.contains("certificate") || message.contains("tls") {
        Some("tls")
    } else if message.contains("proxy") {
        Some("proxy")
    } else if message.contains("timed out") || message.contains("timeout") {
        Some("timeout")
    } else if message.contains("error sending request") {
        Some("connection")
    } else if message.contains("workspace routing discovery failed") {
        Some("workspace_discovery")
    } else {
        None
    };
    result
}

fn classify_rpc_error(method: &str, error: &Value) -> QuotaError {
    if error.get("code").and_then(Value::as_i64) == Some(-32601) {
        let message = match method {
            "account/rateLimitResetCredit/consume" => {
                "This Codex engine does not provide quota reset"
            }
            "account/rateLimits/read" => "This Codex engine does not provide quota reads",
            "account/read" => "This Codex engine does not provide account reads",
            _ => "This Codex engine does not provide the requested operation",
        };
        return QuotaError::new(message, "quota_operation_unavailable");
    }
    let message = error
        .as_object()
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let auth_required = matches!(
        message,
        "codex account authentication required to read rate limits"
            | "chatgpt authentication required to read rate limits"
    ) || (method == "account/rateLimitResetCredit/consume"
        && message
            .to_ascii_lowercase()
            .contains("authentication required"));
    let status = rpc_http_status(message);
    if auth_required || status == Some(401) {
        return QuotaError::new(
            "Codex account needs sign-in; sign in again and re-import the account",
            "quota_auth_required",
        );
    }
    if status == Some(403) {
        return QuotaError::new(
            "Codex quota access was denied (403); check account access and network",
            "quota_access_denied",
        );
    }
    if status == Some(429) {
        return QuotaError::new(
            "Codex quota queries are rate limited (429); try again later",
            "quota_rate_limited",
        );
    }
    if method == "account/read"
        && [
            "workspace routing discovery timed out",
            "workspace routing discovery failed",
        ]
        .iter()
        .any(|expected| message.trim().eq_ignore_ascii_case(expected))
    {
        return QuotaError::new(
            "Codex could not reach ChatGPT workspace routing; check DNS, VPN/TUN, proxy, and network connectivity",
            "quota_transport_error",
        )
        .with_imported_refresh_retry();
    }
    match method {
        "account/rateLimits/read"
            if message
                .to_ascii_lowercase()
                .contains("error sending request") =>
        {
            QuotaError::new(
                "Codex could not connect to the quota service; check the proxy and network connection",
                "quota_transport_error",
            )
        }
        "account/rateLimits/read" => QuotaError::new(
            "Codex quota service query failed; check network connectivity and try again",
            "quota_fetch_failed",
        ),
        "account/read" => QuotaError::new("Codex account read failed", "quota_account_read_failed"),
        "account/rateLimitResetCredit/consume" => QuotaError::new(
            "Codex could not use the reset opportunity",
            "quota_reset_failed",
        ),
        _ => QuotaError::new(
            "Codex app-server initialization failed",
            "quota_initialize_failed",
        ),
    }
}

/// Parse one bounded app-server transcript into the browser-safe quota shape.
pub fn parse_app_server_output(output: &str) -> Result<Value, QuotaError> {
    let observed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    parse_app_server_output_at(output, observed_at)
}

/// Deterministic variant used by the Python differential oracle.
pub fn parse_app_server_output_at(output: &str, observed_at: u64) -> Result<Value, QuotaError> {
    let mut account = Map::new();
    let mut rate_limits = None;
    let mut rate_limits_result = Map::new();
    let mut buckets = Map::new();

    for line in output.lines() {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Some(message) = message.as_object() else {
            return Err(QuotaError::new(
                "Codex quota JSON-RPC output must be an object",
                "quota_output_protocol_error",
            ));
        };
        if let Some(result) = message.get("result").and_then(Value::as_object) {
            if let Some(current) = result.get("account").and_then(Value::as_object) {
                account = current.clone();
            }
            read_limits(
                result,
                &mut rate_limits,
                &mut rate_limits_result,
                &mut buckets,
            );
        }
        if message.get("method").and_then(Value::as_str) == Some("account/rateLimits/updated")
            && let Some(params) = message.get("params").and_then(Value::as_object)
        {
            read_limits(
                params,
                &mut rate_limits,
                &mut rate_limits_result,
                &mut buckets,
            );
        }
    }
    let Some(rate_limits) = rate_limits else {
        return Err(QuotaError::new(
            "Codex did not return account rate limits",
            "quota_error",
        ));
    };
    let plan_type = account
        .get("planType")
        .and_then(Value::as_str)
        .or_else(|| rate_limits.get("planType").and_then(Value::as_str))
        .map_or(Value::Null, |value| Value::String(value.to_owned()));
    let plan_type = if has_thirty_day_quota_window(&buckets) {
        Value::String("free".to_owned())
    } else {
        plan_type
    };
    Ok(json!({
        "account_label": mask_email(account.get("email")),
        "plan_type": plan_type,
        "rate_limits": rate_limits,
        "rate_limits_by_limit_id": buckets,
        "credits": safe_credit_snapshot(&rate_limits, &rate_limits_result),
        "updated_at": observed_at,
    }))
}

fn has_thirty_day_quota_window(buckets: &Map<String, Value>) -> bool {
    buckets.values().any(|bucket| {
        let Some(bucket) = bucket.as_object() else {
            return false;
        };
        ["primary", "secondary"].into_iter().any(|name| {
            let Some(window) = bucket.get(name).and_then(Value::as_object) else {
                return false;
            };
            let duration = window
                .get("windowDurationMins")
                .or_else(|| window.get("window_duration_mins"))
                .or_else(|| window.get("window_minutes"));
            duration.is_some_and(|duration| {
                duration.as_i64() == Some(43_200)
                    || duration.as_u64() == Some(43_200)
                    || duration.as_f64() == Some(43_200.0)
            })
        })
    })
}

/// Return the allowlisted reset outcome for one idempotent reset request.
pub fn reset_outcome(output: &str, request_id: i64) -> Result<&'static str, QuotaError> {
    for line in output.lines() {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Some(message) = message.as_object() else {
            return Err(QuotaError::new(
                "Codex reset JSON-RPC output must be an object",
                "quota_output_protocol_error",
            ));
        };
        if message.get("id").and_then(Value::as_i64) != Some(request_id) {
            continue;
        }
        let outcome = message
            .get("result")
            .and_then(Value::as_object)
            .and_then(|result| result.get("outcome"))
            .and_then(Value::as_str);
        return match outcome {
            Some("reset") => Ok("reset"),
            Some("nothingToReset") => Ok("nothingToReset"),
            Some("noCredit") => Ok("noCredit"),
            Some("alreadyRedeemed") => Ok("alreadyRedeemed"),
            _ => Err(QuotaError::new(
                "Codex did not return a reset outcome",
                "quota_reset_failed",
            )),
        };
    }
    Err(QuotaError::new(
        "Codex did not return a reset outcome",
        "quota_reset_failed",
    ))
}
