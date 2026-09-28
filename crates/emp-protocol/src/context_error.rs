// Bounded, Python-compatible classifier for explicit context-length failures.

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;

use crate::collaboration::python_str;

fn normalized_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_separator = false;
    for byte in text.to_lowercase().chars() {
        if byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == '_' {
            if in_separator {
                result.push('_');
            }
            in_separator = false;
            result.push(byte);
        } else {
            in_separator = true;
        }
    }
    result
}

fn contains_context_marker(compact: &str) -> bool {
    [
        "context_length_exceeded",
        "context_window_exceeded",
        "maximum_context_length_exceeded",
        "prompt_is_too_long",
        "input_too_long",
    ]
    .iter()
    .any(|marker| compact.contains(marker))
}

fn ordered_context_statement(text: &str) -> bool {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN
        .get_or_init(|| {
            Regex::new(
                r"(?:(?:context|prompt|input).{0,32}(?:length|window|token|limit).{0,32}(?:exceed|too\s+long|maximum|max\b))|(?:(?:maximum|max\b|limit).{0,32}(?:context|prompt|input).{0,32}(?:length|window|token))",
            )
            .expect("fixed context error expression is valid")
        })
        .is_match(text)
}

// Upstream error payloads are small; these bounds keep hostile or merely large
// bodies (for example a streamed event carrying a whole response) from making
// classification super-linear. Real error payloads are read with a 4 KiB
// prefix, so the per-text cap does not lose evidence on error paths.
const MAX_EVIDENCE_TEXT_CHARS: usize = 4 * 1024;
const MAX_EVIDENCE_TOTAL_CHARS: usize = 64 * 1024;
const MAX_EVIDENCE_DEPTH: usize = 32;
const MAX_EVIDENCE_NODES: usize = 4096;
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_EVIDENCE_KEY_BYTES: usize = 64;

fn normalized_key(key: &str) -> Option<String> {
    (key.len() <= MAX_EVIDENCE_KEY_BYTES).then(|| key.to_lowercase().replace(['-', ' '], "_"))
}

fn is_marker_key(name: &str) -> bool {
    matches!(name, "code" | "type" | "error_code" | "param" | "reason")
}

fn is_statement_key(name: &str) -> bool {
    matches!(name, "message" | "detail" | "error" | "type" | "code")
}

fn truncated_chars(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(offset, _)| &text[..offset])
}

/// Whether the Python rendering of `value` stays within `limit` characters,
/// visiting at most about `limit` nodes/characters before giving up.
fn renders_within(value: &Value, limit: &mut usize) -> bool {
    let cost = match value {
        Value::String(text) => text.len().saturating_add(2),
        Value::Array(items) => {
            *limit = match limit.checked_sub(2) {
                Some(rest) => rest,
                None => return false,
            };
            return items.iter().all(|item| renders_within(item, limit));
        }
        Value::Object(items) => {
            *limit = match limit.checked_sub(2) {
                Some(rest) => rest,
                None => return false,
            };
            return items.iter().all(|(key, item)| {
                match limit.checked_sub(key.len().saturating_add(4)) {
                    Some(rest) => *limit = rest,
                    None => return false,
                }
                renders_within(item, limit)
            });
        }
        _ => 32,
    };
    match limit.checked_sub(cost) {
        Some(rest) => {
            *limit = rest;
            true
        }
        None => false,
    }
}

/// Lowercased Python `str()` of a value, bounded to the examined prefix.
/// Composite values too large to render cheaply are skipped; their nested
/// keys are still inspected structurally.
fn evidence_text(item: &Value) -> Option<String> {
    match item {
        Value::String(text) => Some(truncated_chars(text, MAX_EVIDENCE_TEXT_CHARS).to_lowercase()),
        Value::Array(_) | Value::Object(_) => {
            let mut limit = MAX_EVIDENCE_TEXT_CHARS;
            renders_within(item, &mut limit)
                .then(|| truncated_chars(&python_str(item), MAX_EVIDENCE_TEXT_CHARS).to_lowercase())
        }
        _ => Some(python_str(item).to_lowercase()),
    }
}

fn context_evidence(value: &Value, depth: usize, budget: &mut usize, nodes: &mut usize) -> bool {
    if depth > MAX_EVIDENCE_DEPTH || *budget == 0 || *nodes == 0 {
        return false;
    }
    *nodes -= 1;
    let Some(object) = value.as_object() else {
        if let Some(values) = value.as_array() {
            for item in values {
                if *nodes == 0 {
                    break;
                }
                if context_evidence(item, depth + 1, budget, nodes) {
                    return true;
                }
            }
        }
        return false;
    };

    for (key, item) in object {
        if *nodes == 0 {
            return false;
        }
        *nodes -= 1;
        let name = normalized_key(key);
        let marker_key = name.as_deref().is_some_and(is_marker_key);
        let statement_key = name.as_deref().is_some_and(is_statement_key);
        if (marker_key || statement_key)
            && let Some(text) = evidence_text(item)
        {
            let Some(rest) = budget.checked_sub(text.len()) else {
                *budget = 0;
                return false;
            };
            *budget = rest;

            if marker_key && contains_context_marker(&normalized_text(&text)) {
                return true;
            }

            if statement_key && ordered_context_statement(&text) {
                return true;
            }
        }

        if context_evidence(item, depth + 1, budget, nodes) {
            return true;
        }
    }

    false
}

/// Classify only structured provider evidence, never generic or WAF HTML.
pub fn is_explicit_context_error(status: u16, content_type: &str, raw: &[u8]) -> bool {
    if !matches!(status, 200 | 400 | 413 | 422) || raw.len() > MAX_ERROR_BODY_BYTES {
        return false;
    }

    let media = content_type.to_lowercase();
    let stripped = raw
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .map(|offset| &raw[offset..])
        .unwrap_or_default();
    if !media.contains("json") && !stripped.starts_with(b"{") && !stripped.starts_with(b"[") {
        return false;
    }

    let text = String::from_utf8_lossy(raw);
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return false;
    };
    let mut budget = MAX_EVIDENCE_TOTAL_CHARS;
    let mut nodes = MAX_EVIDENCE_NODES;
    context_evidence(&value, 0, &mut budget, &mut nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn classify(value: &Value) -> bool {
        is_explicit_context_error(400, "application/json", value.to_string().as_bytes())
    }

    #[test]
    fn examined_text_is_bounded_per_value_and_in_total() {
        let near = format!("{} maximum context length", "x".repeat(1000));
        assert!(classify(&json!({"error": {"message": near}})));

        let far = format!(
            "{} maximum context length",
            "x".repeat(MAX_EVIDENCE_TEXT_CHARS)
        );
        assert!(!classify(&json!({"error": {"message": far}})));

        // Structured markers below an oversized composite are still found.
        let large = json!({"error": {
            "padding": "p".repeat(MAX_EVIDENCE_TEXT_CHARS * 2),
            "code": "context_length_exceeded",
        }});
        assert!(classify(&large));
    }

    #[test]
    fn adversarial_payloads_classify_quickly() {
        let started = std::time::Instant::now();
        let repeated = "context length ".repeat(64 * 1024);
        assert!(!classify(
            &json!({"message": repeated.replace("length", "lenght")})
        ));

        let mut nested = json!({"message": "context ".repeat(512)});
        for _ in 0..100 {
            nested = json!({"error": nested, "type": "x".repeat(512)});
        }
        assert!(!classify(&nested));

        let wide = (0..20_000)
            .map(|index| json!({"type": "output_text", "text": format!("item {index}")}))
            .collect::<Vec<_>>();
        assert!(!classify(&json!({"response": {"output": wide}})));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn evidence_depth_is_capped() {
        let mut deep = json!({"code": "context_length_exceeded"});
        for _ in 0..MAX_EVIDENCE_DEPTH + 4 {
            deep = json!({"wrapper": deep});
        }
        assert!(!classify(&deep));
        let mut shallow = json!({"code": "context_length_exceeded"});
        for _ in 0..8 {
            shallow = json!({"wrapper": shallow});
        }
        assert!(classify(&shallow));
    }
}
