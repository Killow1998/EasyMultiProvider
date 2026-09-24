//! Provider, model and capability normalization.

use super::*;

const PROVIDER_PROTOCOLS: [&str; 4] = [
    "auto",
    "responses",
    "chat_completions",
    "anthropic_messages",
];
const PROVIDER_AUTH_MODES: [&str; 3] = ["api_key", "anthropic_api_key", "forward"];
const CAPABILITY_SOURCES: [(&str, f64); 7] = [
    ("official", 0.95),
    ("advertised", 0.75),
    ("observed", 1.0),
    ("manual", 1.0),
    ("inherited", 0.6),
    ("inferred", 0.35),
    ("unknown", 0.0),
];
const PROVIDER_BOOLEAN_CAPABILITIES: [&str; 8] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "websocket",
];

const MODEL_CAPABILITY_SOURCE_FIELDS: [&str; 17] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "websocket",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "supports_image_detail_original",
];
const MODEL_EXPLICIT_CAPABILITY_FIELDS: [&str; 10] = [
    "supports_reasoning",
    "supports_reasoning_summaries",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "reasoning_control",
    "max_input_tokens",
    "structured_output",
    "web_search",
    "supports_image_detail_original",
];
pub(super) const MODEL_BOOLEAN_CAPABILITIES: [&str; 8] = [
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "websocket",
];
pub(super) const TOP_LEVEL_PROVENANCE_FIELDS: [&str; 11] = [
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "supports_image_detail_original",
];

pub(super) fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

pub(super) fn safe_capability_identity(raw: Option<&Value>, field: &str) -> ConfigResult<String> {
    let value = string_value(raw, field, false)?;
    let valid = value.len() <= 256
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | ':' | '-')
        });
    if !value.is_empty() && !valid {
        return Err(ConfigError::new(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(value)
}

fn normalize_resolved_protocol(raw: Option<&Value>) -> ConfigResult<String> {
    let value = string_value(raw, "resolved_protocol", false)?;
    if !value.is_empty() && !PROVIDER_PROTOCOLS[1..].contains(&value.as_str()) {
        return Err(ConfigError::new(
            "resolved_protocol must be a concrete protocol",
        ));
    }
    Ok(value)
}

fn leap_year(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn valid_iso_date(value: &str) -> bool {
    if value.len() != 10
        || value.as_bytes().get(4) != Some(&b'-')
        || value.as_bytes().get(7) != Some(&b'-')
    {
        return false;
    }
    let Ok(year) = value[..4].parse::<u32>() else {
        return false;
    };
    if year == 0 {
        return false;
    }
    let Ok(month) = value[5..7].parse::<u32>() else {
        return false;
    };
    let Ok(day) = value[8..].parse::<u32>() else {
        return false;
    };
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day)
}

fn two_digit_number(value: &str, maximum: u32) -> bool {
    value.len() == 2
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u32>().is_ok_and(|number| number <= maximum)
}

fn strip_fraction(value: &str) -> Option<&str> {
    let separator = value.find(['.', ',']);
    match separator {
        None => Some(value),
        Some(index) => {
            let fraction = &value[index + 1..];
            (!fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit()))
                .then_some(&value[..index])
        }
    }
}

fn valid_iso_clock(value: &str) -> bool {
    let Some(base) = strip_fraction(value) else {
        return false;
    };
    let fields = if base.contains(':') {
        let fields = base.split(':').collect::<Vec<_>>();
        if !(1..=3).contains(&fields.len()) {
            return false;
        }
        fields
    } else {
        match base.len() {
            2 => vec![&base[..2]],
            4 => vec![&base[..2], &base[2..4]],
            6 => vec![&base[..2], &base[2..4], &base[4..6]],
            _ => return false,
        }
    };
    two_digit_number(fields[0], 23)
        && fields
            .get(1)
            .is_none_or(|minute| two_digit_number(minute, 59))
        && fields
            .get(2)
            .is_none_or(|second| two_digit_number(second, 59))
}

fn valid_iso_time(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    let (time, offset) = if let Some(time) = value.strip_suffix('Z') {
        if time.contains('Z') {
            return false;
        }
        (time, None)
    } else if value.contains('Z') {
        return false;
    } else if let Some(index) = value
        .char_indices()
        .skip(1)
        .find_map(|(index, character)| matches!(character, '+' | '-').then_some(index))
    {
        (&value[..index], Some(&value[index + 1..]))
    } else {
        (value, None)
    };
    valid_iso_clock(time) && offset.is_none_or(valid_iso_clock)
}

fn weekday(year: u32, month: u32, day: u32) -> u32 {
    const MONTH_OFFSETS: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    let adjusted_year = if month < 3 { year - 1 } else { year };
    (adjusted_year + adjusted_year / 4 - adjusted_year / 100
        + adjusted_year / 400
        + MONTH_OFFSETS[(month - 1) as usize]
        + day)
        % 7
}

fn valid_iso_week_date(value: &str) -> bool {
    let (year, week, day) = match value.len() {
        7 if value.as_bytes().get(4) == Some(&b'W') => (&value[..4], &value[5..7], "1"),
        8 if value.as_bytes().get(4) == Some(&b'W') => (&value[..4], &value[5..7], &value[7..8]),
        8 if value.get(4..6) == Some("-W") => (&value[..4], &value[6..8], "1"),
        10 if value.get(4..6) == Some("-W") && value.as_bytes().get(8) == Some(&b'-') => {
            (&value[..4], &value[6..8], &value[9..10])
        }
        _ => return false,
    };
    let Ok(year) = year.parse::<u32>() else {
        return false;
    };
    let Ok(week) = week.parse::<u32>() else {
        return false;
    };
    let Ok(day) = day.parse::<u32>() else {
        return false;
    };
    if year == 0 || !(1..=7).contains(&day) {
        return false;
    }
    let jan_first = weekday(year, 1, 1);
    let maximum_week = if jan_first == 4 || (jan_first == 3 && leap_year(year)) {
        53
    } else {
        52
    };
    (1..=maximum_week).contains(&week)
}

fn valid_basic_iso_date(value: &str) -> bool {
    value.len() == 8
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && valid_iso_date(&format!(
            "{}-{}-{}",
            &value[..4],
            &value[4..6],
            &value[6..8]
        ))
}

fn python_iso_date_prefix_length(value: &str) -> Option<usize> {
    for length in [10usize, 8, 7] {
        let Some(candidate) = value.get(..length) else {
            continue;
        };
        if valid_iso_date(candidate)
            || valid_basic_iso_date(candidate)
            || valid_iso_week_date(candidate)
        {
            return Some(length);
        }
    }
    None
}

pub(super) fn valid_python_iso_timestamp(value: &str) -> bool {
    let Some(date_length) = python_iso_date_prefix_length(value) else {
        return false;
    };
    let Some(rest) = value.get(date_length..) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let mut characters = rest.chars();
    let Some(separator) = characters.next() else {
        return false;
    };
    if separator == 'Z' {
        return false;
    }
    valid_iso_time(characters.as_str())
}

fn protocol_confidence(raw: Option<&Value>, default: f64) -> ConfigResult<f64> {
    let value = match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => f64::from(*value),
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| ConfigError::new("invalid protocol_observation"))?,
        Some(Value::String(value)) => value
            .trim()
            .parse::<f64>()
            .map_err(|_| ConfigError::new("invalid protocol_observation"))?,
        Some(_) => return Err(ConfigError::new("invalid protocol_observation")),
    };
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::new("invalid protocol_observation"));
    }
    Ok(value)
}

fn normalize_protocol_observation(raw: Option<&Value>) -> ConfigResult<Value> {
    let empty = Map::new();
    let raw = match raw {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new("protocol_observation must be an object"));
        }
    };
    let source = match raw.get("source") {
        None => "unknown",
        Some(Value::String(value)) => value,
        Some(_) => return Err(ConfigError::new("invalid protocol_observation")),
    };
    let Some((_, default_confidence)) = CAPABILITY_SOURCES
        .iter()
        .find(|(candidate, _)| *candidate == source)
    else {
        return Err(ConfigError::new("invalid protocol_observation"));
    };
    let confidence = protocol_confidence(raw.get("confidence"), *default_confidence)?;
    let observed_at = match raw.get("observed_at") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(value)) if valid_python_iso_timestamp(value) => {
            Value::String(value.clone())
        }
        Some(_) => return Err(ConfigError::new("invalid protocol_observation")),
    };
    let fingerprint = string_value(
        raw.get("endpoint_fingerprint"),
        "protocol_observation.endpoint_fingerprint",
        false,
    )?;
    let valid_fingerprint = fingerprint.len() == 71
        && fingerprint.starts_with("sha256:")
        && fingerprint[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
    if !fingerprint.is_empty() && !valid_fingerprint {
        return Err(ConfigError::new(
            "protocol_observation.endpoint_fingerprint is invalid",
        ));
    }
    Ok(serde_json::json!({
        "source": source,
        "confidence": confidence,
        "observed_at": observed_at,
        "endpoint_fingerprint": fingerprint,
        "deployment_identity": safe_capability_identity(
            raw.get("deployment_identity"),
            "protocol_observation.deployment_identity",
        )?,
        "upstream_model": safe_capability_identity(
            raw.get("upstream_model"),
            "protocol_observation.upstream_model",
        )?,
    }))
}

pub(super) fn python_capability_trim(value: &str) -> &str {
    value.trim_matches(|character: char| {
        matches!(
            character,
            '\t'
                | '\n'
                | '\u{b}'
                | '\u{c}'
                | '\u{d}'
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

fn effective_model_capability_value<'a>(values: &'a Value, field: &str) -> Option<&'a Value> {
    if let Some(value) = values.get(field) {
        return Some(value);
    }
    values
        .get("capabilities")
        .filter(|value| value.is_object())
        .and_then(|capabilities| capabilities.get(field))
}

fn model_capability_known(field: &str, value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return false;
    };
    if MODEL_BOOLEAN_CAPABILITIES.contains(&field) {
        return value.is_boolean();
    }
    match field {
        "reasoning_levels" => value.as_array().is_some_and(|levels| !levels.is_empty()),
        "reasoning_control" => value
            .as_str()
            .is_some_and(|control| !python_capability_trim(control).is_empty()),
        "input_modalities" => input_modalities_known(Some(value)),
        "output_modalities" => output_modalities_known(Some(value)),
        "supported_protocols" => supported_protocols_known(Some(value)),
        "supports_image_detail_original" => value.is_boolean(),
        _ => {
            value.as_i64().is_some_and(|number| number > 0)
                || value.as_u64().is_some_and(|number| number > 0)
        }
    }
}

fn default_model_capability_source(
    field: &str,
    values: &Value,
    explicit_fields: Option<&[&str]>,
) -> &'static str {
    if MODEL_EXPLICIT_CAPABILITY_FIELDS.contains(&field) {
        return if explicit_fields.is_some_and(|fields| fields.contains(&field)) {
            "manual"
        } else {
            "unknown"
        };
    }
    if model_capability_known(field, effective_model_capability_value(values, field)) {
        "inferred"
    } else {
        "unknown"
    }
}

fn model_provenance_confidence(raw: Option<&Value>, default: f64) -> ConfigResult<f64> {
    let value = match raw {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => f64::from(*value),
        Some(Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| ConfigError::new("invalid provenance value"))?,
        Some(Value::String(value)) => value
            .trim()
            .parse::<f64>()
            .map_err(|_| ConfigError::new("invalid provenance value"))?,
        Some(_) => return Err(ConfigError::new("invalid provenance value")),
    };
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::new("invalid provenance value"));
    }
    Ok(value)
}

fn normalize_model_provenance(
    raw: &Map<String, Value>,
    default_source: &str,
) -> ConfigResult<Value> {
    let source = match raw.get("source") {
        None => default_source,
        Some(Value::String(source)) => source,
        Some(_) => return Err(ConfigError::new("unsupported capability source")),
    };
    let Some((_, default_confidence)) = CAPABILITY_SOURCES
        .iter()
        .find(|(candidate, _)| candidate == &source)
    else {
        return Err(ConfigError::new("unsupported capability source"));
    };
    let confidence = model_provenance_confidence(raw.get("confidence"), *default_confidence)?;
    let observed_at = match raw.get("observed_at") {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(value)) if valid_python_iso_timestamp(value) => {
            Value::String(value.clone())
        }
        Some(_) => return Err(ConfigError::new("invalid capability observed_at")),
    };
    Ok(serde_json::json!({
        "source": source,
        "confidence": confidence,
        "observed_at": observed_at,
    }))
}

/// Normalize capability provenance for an already-normalized model record.
///
/// `values` must have the shape produced by Python `_normalize_model`; in
/// particular, top-level capability values take precedence over the nested
/// `capabilities` object even when the top-level value is null. Explicitness is
/// raw model-key presence and is supplied by the caller.
pub fn normalize_model_capability_sources(
    raw: Option<&Value>,
    values: &Value,
    explicit_fields: Option<&[&str]>,
) -> ConfigResult<Value> {
    if !values.is_object() {
        return Err(ConfigError::new(
            "model.capability_sources requires normalized model values",
        ));
    }
    let empty = Map::new();
    let raw = match raw {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(
                "model.capability_sources must be an object",
            ));
        }
    };
    let mut result = Map::new();
    for field in MODEL_CAPABILITY_SOURCE_FIELDS {
        let Some(value) = raw.get(field) else {
            if model_capability_known(field, effective_model_capability_value(values, field)) {
                let source = default_model_capability_source(field, values, explicit_fields);
                let provenance = normalize_model_provenance(&Map::new(), source).map_err(|_| {
                    ConfigError::new(format!("invalid provenance for model.{field}"))
                })?;
                result.insert(field.to_owned(), provenance);
            }
            continue;
        };
        let provenance = if let Some(object) = value.as_object() {
            let source = default_model_capability_source(field, values, explicit_fields);
            normalize_model_provenance(object, source)
                .map_err(|_| ConfigError::new(format!("invalid provenance for model.{field}")))?
        } else {
            return Err(ConfigError::new(format!(
                "model.capability_sources.{field} must be an object"
            )));
        };
        result.insert(field.to_owned(), provenance);
    }
    Ok(Value::Object(result))
}

fn strict_reasoning_levels(raw: Option<&Value>) -> ConfigResult<Vec<String>> {
    let default = Value::Array(Vec::new());
    let raw = raw.unwrap_or(&default);
    let Value::Array(items) = raw else {
        return Err(ConfigError::new("model.reasoning_levels must be a list"));
    };
    let mut levels = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(level) = item else {
            return Err(ConfigError::new("model.reasoning_levels must be a string"));
        };
        levels.push(python_capability_trim(level).to_owned());
    }
    if levels.iter().any(|level| level.is_empty()) {
        return Err(ConfigError::new(
            "model.reasoning_levels entries must not be empty",
        ));
    }
    Ok(normalize_reasoning_levels(Some(&Value::Array(
        levels.into_iter().map(Value::String).collect(),
    ))))
}

fn broad_model_id(raw: Option<&Value>, field: &str) -> ConfigResult<String> {
    let value = string_value(raw, field, true)?;
    let valid = !value.is_empty()
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | ':' | '-')
        });
    if !valid {
        return Err(ConfigError::new(format!(
            "{field} contains unsupported characters"
        )));
    }
    Ok(value)
}

fn normalized_capability_object(raw: Option<&Value>, field: &str) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Object(Map::new())),
        Some(Value::Object(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(format!("{field} must be an object")));
        }
    };
    let mut normalized = Map::new();
    for name in MODEL_BOOLEAN_CAPABILITIES {
        let Some(value) = raw.get(name) else {
            continue;
        };
        let Value::Bool(value) = value else {
            return Err(ConfigError::new(format!("{field}.{name} must be boolean")));
        };
        normalized.insert((*name).to_owned(), Value::Bool(*value));
    }
    Ok(Value::Object(normalized))
}

mod model;
pub use model::normalize_model;
mod provider;
pub use provider::normalize_provider;
