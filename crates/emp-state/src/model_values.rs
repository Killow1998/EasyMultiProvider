//! Exact, bounded capability value normalization shared with Python.
//!
//! Modality identifiers are normalized with Python's Unicode whitespace and
//! case rules before validation. Protocol and reasoning values retain their
//! input order except for the explicit reasoning progression ordering.

use serde_json::Value;

pub const MAX_MODALITIES: usize = 16;
pub const MAX_MODALITY_ID_BYTES: usize = 64;
pub const TEXT_MODALITY: &str = "text";
pub const IMAGE_MODALITY: &str = "image";
const KNOWN_CONCRETE_PROTOCOLS: [&str; 3] = ["responses", "chat_completions", "anthropic_messages"];
const REASONING_EFFORT_ORDER: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// Apply Python `str.strip()` semantics, including the four C0 file separators.
fn python_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(
            character,
            '\t'
                | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\r'
                | ' '
                | '\u{85}'
                | '\u{a0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{1c}'..='\u{1f}'
        )
    })
}

fn python_lower(value: &str) -> String {
    value.to_lowercase()
}

fn modality_identifier(item: &str) -> Option<String> {
    let identifier = python_lower(python_trim(item));
    if identifier.is_empty() || identifier.len() > MAX_MODALITY_ID_BYTES {
        return None;
    }
    let mut characters = identifier.chars();
    let valid = characters
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && characters.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | ':' | '-')
        });
    if valid { Some(identifier) } else { None }
}

fn parse_modalities(value: Option<&Value>) -> Option<Vec<String>> {
    let value = value?;
    let Value::Array(items) = value else {
        return None;
    };
    if items.is_empty() || items.len() > MAX_MODALITIES {
        return None;
    }
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(item) = item else {
            return None;
        };
        let identifier = modality_identifier(item)?;
        if !result.contains(&identifier) {
            result.push(identifier);
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Normalize bounded modality identifiers, falling back to `["text"]`.
pub fn normalize_input_modalities(value: Option<&Value>) -> Vec<String> {
    parse_modalities(value).unwrap_or_else(|| vec![TEXT_MODALITY.to_owned()])
}

/// Return whether a discovery payload contains a valid input modality list.
pub fn input_modalities_known(value: Option<&Value>) -> bool {
    parse_modalities(value).is_some()
}

/// Classify a valid modality list as advertised and all other input as unknown.
pub fn input_modalities_metadata_source(value: Option<&Value>) -> &'static str {
    if input_modalities_known(value) {
        "advertised"
    } else {
        "unknown"
    }
}

/// Project input modalities onto Codex's text/image catalog contract.
pub fn codex_input_modalities(value: Option<&Value>) -> Vec<String> {
    let normalized = normalize_input_modalities(value);
    let projected = [TEXT_MODALITY, IMAGE_MODALITY]
        .into_iter()
        .filter(|item| normalized.iter().any(|candidate| candidate == item))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if projected.is_empty() {
        vec![TEXT_MODALITY.to_owned()]
    } else {
        projected
    }
}

/// Normalize bounded output modality identifiers, falling back to `["text"]`.
pub fn normalize_output_modalities(value: Option<&Value>) -> Vec<String> {
    parse_modalities(value).unwrap_or_else(|| vec![TEXT_MODALITY.to_owned()])
}

/// Return whether a discovery payload contains a valid output modality list.
pub fn output_modalities_known(value: Option<&Value>) -> bool {
    parse_modalities(value).is_some()
}

/// Classify a valid output list as advertised and all other input as unknown.
pub fn output_modalities_metadata_source(value: Option<&Value>) -> &'static str {
    if output_modalities_known(value) {
        "advertised"
    } else {
        "unknown"
    }
}

/// Normalize unique known protocol values, excluding `auto`.
pub fn normalize_supported_protocols(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for item in items {
        let Value::String(item) = item else {
            continue;
        };
        let protocol = python_lower(python_trim(item));
        if KNOWN_CONCRETE_PROTOCOLS.contains(&protocol.as_str()) && !result.contains(&protocol) {
            result.push(protocol);
        }
    }
    result
}

/// Return whether a valid non-empty concrete protocol list is present.
pub fn supported_protocols_known(value: Option<&Value>) -> bool {
    !normalize_supported_protocols(value).is_empty()
}

/// Return unique reasoning efforts in official order, then input order.
pub fn normalize_reasoning_levels(value: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for item in items {
        let Value::String(item) = item else {
            continue;
        };
        let trimmed = python_trim(item);
        if trimmed.is_empty() {
            continue;
        }
        let lowered = python_lower(trimmed);
        let effort = if REASONING_EFFORT_ORDER.contains(&lowered.as_str()) {
            lowered
        } else {
            trimmed.to_owned()
        };
        if !result.contains(&effort) {
            result.push(effort);
        }
    }
    let unknown_rank = REASONING_EFFORT_ORDER.len();
    let mut indexed = result.into_iter().enumerate().collect::<Vec<_>>();
    indexed.sort_by_key(|(index, effort)| {
        (
            REASONING_EFFORT_ORDER
                .iter()
                .position(|known| known == effort)
                .unwrap_or(unknown_rank),
            *index,
        )
    });
    indexed.into_iter().map(|(_, effort)| effort).collect()
}
