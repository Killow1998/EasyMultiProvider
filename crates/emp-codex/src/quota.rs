//! Safe projection of Codex app-server quota JSON-RPC output.

use serde_json::{Map, Value, json};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaError {
    message: String,
    code: &'static str,
}

impl QuotaError {
    fn new(message: impl Into<String>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for QuotaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for QuotaError {}

/// Classify one JSON-RPC error without exposing its upstream URL or body.
pub fn quota_rpc_error(method: &str, error: &Value) -> QuotaError {
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
    Ok(json!({
        "account_label": mask_email(account.get("email")),
        "plan_type": plan_type,
        "rate_limits": rate_limits,
        "rate_limits_by_limit_id": buckets,
        "credits": safe_credit_snapshot(&rate_limits, &rate_limits_result),
        "updated_at": observed_at,
    }))
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

fn rpc_http_status(message: &str) -> Option<u16> {
    let public = message
        .split_once("; body=")
        .map_or(message, |(head, _)| head);
    for (index, _) in public.match_indices(" failed: ") {
        let tail = &public[index + " failed: ".len()..];
        let bytes = tail.as_bytes();
        if bytes.len() >= 3
            && bytes[..3].iter().all(u8::is_ascii_digit)
            && bytes.get(3).is_none_or(|byte| !byte.is_ascii_digit())
            && public[index..].contains("; content-type=")
        {
            return tail[..3].parse().ok();
        }
    }
    None
}

fn read_limits(
    payload: &Map<String, Value>,
    rate_limits: &mut Option<Map<String, Value>>,
    rate_limits_result: &mut Map<String, Value>,
    buckets: &mut Map<String, Value>,
) {
    if let Some(by_id) = payload
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
    {
        let current = by_id
            .iter()
            .filter(|(limit_id, value)| !limit_id.is_empty() && value.is_object())
            .map(|(limit_id, value)| (limit_id.clone(), value.clone()))
            .collect::<Map<_, _>>();
        if !current.is_empty() {
            *buckets = current;
            *rate_limits = buckets
                .get("codex")
                .or_else(|| buckets.values().next())
                .and_then(Value::as_object)
                .cloned();
            *rate_limits_result = payload.clone();
            return;
        }
    }
    if let Some(candidate) = payload.get("rateLimits").and_then(Value::as_object) {
        let limit_id = candidate
            .get("limitId")
            .or_else(|| candidate.get("limit_id"))
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned();
        buckets.insert(limit_id, Value::Object(candidate.clone()));
        *rate_limits = buckets
            .get("codex")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| Some(candidate.clone()));
        *rate_limits_result = payload.clone();
    }
}

fn mask_email(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_str) else {
        return String::new();
    };
    let Some((local, domain)) = value.split_once('@') else {
        return String::new();
    };
    let first = local
        .chars()
        .next()
        .map_or("*".to_owned(), |value| value.to_string());
    format!("{first}***@{domain}")
}

fn safe_credit_snapshot(rate_limits: &Map<String, Value>, result: &Map<String, Value>) -> Value {
    let mut snapshot = Map::new();
    if let Some(credits) = rate_limits.get("credits").and_then(Value::as_object) {
        copy_renamed_scalar(credits, &mut snapshot, "hasCredits", "has_credits");
        copy_renamed_scalar(credits, &mut snapshot, "unlimited", "unlimited");
        copy_renamed_scalar(credits, &mut snapshot, "balance", "balance");
    }
    if let Some(limit) = rate_limits
        .get("individualLimit")
        .and_then(Value::as_object)
    {
        let mut safe = Map::new();
        for (source, target) in [
            ("limit", "limit"),
            ("used", "used"),
            ("remainingPercent", "remaining_percent"),
            ("resetsAt", "resets_at"),
        ] {
            if let Some(value) = limit.get(source).filter(|value| safe_scalar(value))
                && !value.is_null()
            {
                safe.insert(target.to_owned(), value.clone());
            }
        }
        if !safe.is_empty() {
            snapshot.insert("individual_limit".to_owned(), Value::Object(safe));
        }
    }
    if let Some(value) = rate_limits
        .get("spendControlReached")
        .filter(|value| value.is_boolean())
    {
        snapshot.insert("spend_control_reached".to_owned(), value.clone());
    }
    if let Some(value) = safe_reset_credits(result.get("rateLimitResetCredits")) {
        snapshot.insert("reset_credits".to_owned(), value);
    }
    if snapshot.is_empty() {
        Value::Null
    } else {
        Value::Object(snapshot)
    }
}

fn safe_reset_credits(value: Option<&Value>) -> Option<Value> {
    let value = value?.as_object()?;
    let mut snapshot = Map::new();
    if let Some(count) = value.get("availableCount").and_then(Value::as_i64) {
        snapshot.insert("available_count".to_owned(), Value::from(count));
    }
    if let Some(credits) = value.get("credits") {
        if credits.is_null() {
            snapshot.insert("credits".to_owned(), Value::Null);
        } else if let Some(credits) = credits.as_array() {
            let projected = credits
                .iter()
                .filter_map(Value::as_object)
                .map(|credit| {
                    let mut safe = Map::new();
                    for (source, target) in [
                        ("resetType", "reset_type"),
                        ("status", "status"),
                        ("grantedAt", "granted_at"),
                        ("expiresAt", "expires_at"),
                        ("title", "title"),
                        ("description", "description"),
                    ] {
                        if let Some(value) = credit.get(source).filter(|value| safe_scalar(value)) {
                            safe.insert(target.to_owned(), value.clone());
                        }
                    }
                    Value::Object(safe)
                })
                .collect();
            snapshot.insert("credits".to_owned(), Value::Array(projected));
        }
    }
    (!snapshot.is_empty()).then_some(Value::Object(snapshot))
}

fn copy_renamed_scalar(
    source: &Map<String, Value>,
    target: &mut Map<String, Value>,
    source_name: &str,
    target_name: &str,
) {
    if let Some(value) = source.get(source_name).filter(|value| safe_scalar(value)) {
        target.insert(target_name.to_owned(), value.clone());
    }
}

fn safe_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}
