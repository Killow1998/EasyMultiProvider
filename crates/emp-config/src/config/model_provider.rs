use super::*;
use time::{Date, Duration, Month, Weekday};

const PROVIDER_PROTOCOLS: [&str; 4] = [
    "auto",
    "responses",
    "chat_completions",
    "anthropic_messages",
];
const PROVIDER_AUTH_MODES: [&str; 7] = [
    "api_key",
    "anthropic_api_key",
    "forward",
    "claude_login",
    "vertex_adc",
    "vertex_service_account",
    "vertex_api_key",
];
const CAPABILITY_SOURCES: [(&str, f64); 8] = [
    ("catalog", 0.7),
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
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '/' | ':' | '-' | '@')
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

fn parse_iso_week(year: i32, week: &str, day: &str) -> Option<i32> {
    let day: i32 = day.parse().ok()?;
    if !(1..=7).contains(&day) {
        return None;
    }
    let monday = Date::from_iso_week_date(year, week.parse().ok()?, Weekday::Monday).ok()?;
    Some(monday.to_julian_day() + day - 1)
}

fn parse_iso_date(value: &str) -> Option<i32> {
    let (year, rest) = value.split_at_checked(4)?;
    let year: u16 = year.parse().ok()?;
    if year == 0 {
        return None;
    }
    let year = i32::from(year);
    let (month, day) = match rest.as_bytes() {
        [b'W', _, _] => return parse_iso_week(year, &rest[1..], "1"),
        [b'W', _, _, _] => return parse_iso_week(year, rest.get(1..3)?, &rest[3..]),
        [b'-', b'W', _, _] => return parse_iso_week(year, &rest[2..], "1"),
        [b'-', b'W', _, _, b'-', _] => return parse_iso_week(year, &rest[2..4], &rest[5..]),
        [b'-', _, _, b'-', _, _] => (rest.get(1..3)?, &rest[4..]),
        [_, _, _, _] if value.bytes().all(|byte| byte.is_ascii_digit()) => rest.split_at(2),
        _ => return None,
    };
    let month = Month::try_from(month.parse::<u8>().ok()?).ok()?;
    let date = Date::from_calendar_date(year, month, day.parse().ok()?).ok()?;
    Some(date.to_julian_day())
}

fn parse_iso_clock(value: &str) -> Option<Duration> {
    let (base, fractional_time) = if let Some((base, fraction)) = value.split_once(['.', ',']) {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        let seconds_per_unit = match base.len() {
            2 => 3_600.0,
            4 | 5 => 60.0,
            _ => 1.0,
        };
        let fraction = format!("0.{fraction}").parse::<f64>().ok()?;
        (base, Duration::seconds_f64(fraction * seconds_per_unit))
    } else {
        (value, Duration::ZERO)
    };
    let (hour, minute, second) = if base.contains(':') {
        let mut fields = base.split(':');
        let clock = (
            fields.next()?,
            fields.next().unwrap_or("00"),
            fields.next().unwrap_or("00"),
        );
        if fields.next().is_some() {
            return None;
        }
        clock
    } else if base.is_ascii() {
        match base.len() {
            2 => (base, "00", "00"),
            4 => (&base[..2], &base[2..], "00"),
            6 => (&base[..2], &base[2..4], &base[4..]),
            _ => return None,
        }
    } else {
        return None;
    };
    let two_digits = |value: &str, maximum| {
        if value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value
            .parse::<i64>()
            .ok()
            .filter(|&number| number <= maximum)
    };
    Some(
        Duration::hours(two_digits(hour, 23)?)
            + Duration::minutes(two_digits(minute, 59)?)
            + Duration::seconds(two_digits(second, 59)?)
            + fractional_time,
    )
}

pub(super) fn parse_observed_at(value: &str) -> Option<Duration> {
    let (date, rest) = [10, 8, 7].into_iter().find_map(|length| {
        let (date, rest) = value.split_at_checked(length)?;
        Some((Duration::days(i64::from(parse_iso_date(date)?)), rest))
    })?;
    if rest.is_empty() {
        return Some(date);
    }
    let mut characters = rest.chars();
    if characters.next()? == 'Z' {
        return None;
    }
    let time = characters.as_str();
    if let Some(clock) = time.strip_suffix('Z') {
        return Some(date + parse_iso_clock(clock)?);
    }
    if let Some(index) = time.find(['+', '-']) {
        let offset = parse_iso_clock(&time[index + 1..])?;
        let clock = parse_iso_clock(&time[..index])?;
        return Some(if time.as_bytes()[index] == b'+' {
            date + clock - offset
        } else {
            date + clock + offset
        });
    }
    Some(date + parse_iso_clock(time)?)
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
        Some(Value::String(value)) if parse_observed_at(value).is_some() => {
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
            .is_some_and(|control| !trim_whitespace(control).is_empty()),
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
        Some(Value::String(value)) if parse_observed_at(value).is_some() => {
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

/// Top-level values, including null, override nested capabilities;
/// `explicit_fields` records raw top-level and nested key presence.
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
        let mut provenance = if let Some(object) = value.as_object() {
            let source = default_model_capability_source(field, values, explicit_fields);
            normalize_model_provenance(object, source)
                .map_err(|_| ConfigError::new(format!("invalid provenance for model.{field}")))?
        } else {
            return Err(ConfigError::new(format!(
                "model.capability_sources.{field} must be an object"
            )));
        };
        if field == "context_window" && value["input_only"] == true {
            provenance["input_only"] = true.into();
        }
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
        levels.push(trim_whitespace(level).to_owned());
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
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '_' | '/' | ':' | '-' | '@')
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
