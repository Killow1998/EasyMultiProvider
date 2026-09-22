// Bounded, Python-compatible classifier for explicit context-length failures.

use serde_json::Value;

use crate::collaboration::python_str;

fn normalized_key(key: &str) -> String {
    key.to_lowercase().replace(['-', ' '], "_")
}

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

fn literal_at(text: &[char], position: usize, word: &str) -> Option<usize> {
    let end = position + word.len();
    (end <= text.len() && text[position..end].iter().copied().eq(word.chars())).then_some(end)
}

fn max_at(text: &[char], position: usize) -> Option<usize> {
    literal_at(text, position, "maximum").or_else(|| {
        let end = literal_at(text, position, "max")?;
        (!text
            .get(end)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_'))
        .then_some(end)
    })
}

fn too_long_at(text: &[char], position: usize) -> Option<usize> {
    let start = literal_at(text, position, "too")?;
    let mut end = start;
    while text
        .get(end)
        .is_some_and(|c| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(c))
    {
        end += 1;
    }
    if end == start {
        None
    } else {
        literal_at(text, end, "long")
    }
}

fn gap_positions(text: &[char], start: usize) -> impl Iterator<Item = usize> + '_ {
    (start..=text.len().min(start + 32))
        .take_while(move |end| *end == start || text[*end - 1] != '\n')
}

fn ordered_context_statement(text: &str) -> bool {
    // Python regex gaps count Unicode code points, permit backtracking, and
    // exclude only LF; max requires a Unicode word boundary and too\s+long
    // accepts more than one whitespace character.
    let text = text.chars().collect::<Vec<_>>();
    for position in 0..text.len() {
        for subject in ["context", "prompt", "input"] {
            if let Some(end) = literal_at(&text, position, subject) {
                for measure_at in gap_positions(&text, end) {
                    for measure in ["length", "window", "token", "limit"] {
                        if let Some(end) = literal_at(&text, measure_at, measure) {
                            for action in gap_positions(&text, end) {
                                if literal_at(&text, action, "exceed").is_some()
                                    || max_at(&text, action).is_some()
                                    || too_long_at(&text, action).is_some()
                                {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(end) = max_at(&text, position).or_else(|| literal_at(&text, position, "limit"))
        {
            for subject_at in gap_positions(&text, end) {
                for subject in ["context", "prompt", "input"] {
                    if let Some(end) = literal_at(&text, subject_at, subject) {
                        for measure_at in gap_positions(&text, end) {
                            if ["length", "window", "token"]
                                .iter()
                                .any(|measure| literal_at(&text, measure_at, measure).is_some())
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

fn context_evidence(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return value
            .as_array()
            .is_some_and(|values| values.iter().any(context_evidence));
    };

    for (key, item) in object {
        let name = normalized_key(key);
        let text = python_str(item).to_lowercase();
        let compact = normalized_text(&text);

        if matches!(
            name.as_str(),
            "code" | "type" | "error_code" | "param" | "reason"
        ) && contains_context_marker(&compact)
        {
            return true;
        }

        if matches!(
            name.as_str(),
            "message" | "detail" | "error" | "type" | "code"
        ) && ordered_context_statement(&text)
        {
            return true;
        }

        if context_evidence(item) {
            return true;
        }
    }

    false
}

/// Classify only structured provider evidence, never generic or WAF HTML.
pub fn is_explicit_context_error(status: u16, content_type: &str, raw: &[u8]) -> bool {
    if !matches!(status, 200 | 400 | 413 | 422) {
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
    context_evidence(&value)
}
