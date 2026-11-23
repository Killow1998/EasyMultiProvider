//! Decode and redact untrusted upstream HTTP rejection bodies.
use emp_domain::json_truthy;
use serde_json::Value;

pub(crate) fn details(status: u16, content_type: &str, raw: &[u8]) -> (Option<String>, String) {
    let media = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let decoded = String::from_utf8_lossy(raw);
    let stripped = decoded.trim_start().to_lowercase();
    if matches!(media.as_str(), "text/html" | "application/xhtml+xml")
        || stripped.starts_with("<html")
        || stripped.starts_with("<!doctype html")
    {
        return (
            None,
            format!("HTTP {status}: upstream returned an HTML error page."),
        );
    }
    let mut detail = decoded.to_string();
    let mut code = None;
    let mut parameter = None;
    if (media.contains("json") || stripped.starts_with(['{', '[']))
        && let Ok(value) = serde_json::from_str::<Value>(&decoded)
    {
        let nested = if value.is_object() {
            value
                .get("error")
                .filter(|value| json_truthy(value))
                .or_else(|| value.get("message").filter(|value| json_truthy(value)))
                .or_else(|| value.get("detail").filter(|value| json_truthy(value)))
                .unwrap_or(&value)
        } else {
            &value
        };
        code = nested
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| nested.get("type").and_then(Value::as_str))
            .map(str::to_owned);
        parameter = nested
            .get("param")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let nested = if nested.is_object() {
            ["message", "type", "code"]
                .iter()
                .find_map(|key| nested.get(key).filter(|value| json_truthy(value)))
                .unwrap_or(nested)
        } else {
            nested
        };
        detail = nested
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| "Upstream returned an error without a readable message.".to_owned());
    }
    let code = code.map(|value| emp_state::diagnostics::redact_sensitive_text(&value));
    let mut metadata = Vec::new();
    if let Some(code) = &code {
        metadata.push(format!("code: {code}"));
    }
    if let Some(parameter) = parameter {
        metadata.push(format!("parameter: {parameter}"));
    }
    let metadata = if metadata.is_empty() {
        String::new()
    } else {
        format!(" [{}]", metadata.join("; "))
    };
    if detail.trim().is_empty() {
        detail = "Upstream returned an empty error response.".to_owned();
    }
    detail = emp_state::diagnostics::redact_sensitive_text(&format!(
        "HTTP {status}{metadata}: {detail}"
    ));
    detail = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    if detail.chars().count() > 512 {
        detail = detail
            .chars()
            .take(509)
            .collect::<String>()
            .trim_end()
            .to_owned()
            + "...";
    }
    (code, detail)
}
