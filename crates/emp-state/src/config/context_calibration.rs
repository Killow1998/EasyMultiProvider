//! Python-compatible numeric parsing and model context calibration.

use super::{
    ConfigError, ConfigResult, Map, Value, json_truthy, python_capability_trim,
    safe_capability_identity, string_value, valid_python_iso_timestamp,
};

const CONTEXT_CALIBRATION_LIMIT: usize = 8;
const CONTEXT_CALIBRATION_PROTOCOLS: [&str; 3] =
    ["responses", "chat_completions", "anthropic_messages"];
pub(super) const MAX_CONTEXT_WINDOW: i64 = 100_000_000;

fn unicode_decimal_digit(character: char) -> Option<u8> {
    const ZEROES: [u32; 76] = [
        0x30, 0x660, 0x6f0, 0x7c0, 0x966, 0x9e6, 0xa66, 0xae6, 0xb66, 0xbe6, 0xc66, 0xce6, 0xd66,
        0xde6, 0xe50, 0xed0, 0xf20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946, 0x19d0, 0x1a80, 0x1a90,
        0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0, 0xa9f0, 0xaa50, 0xabf0,
        0xff10, 0x104a0, 0x10d30, 0x10d40, 0x11066, 0x110f0, 0x11136, 0x111d0, 0x112f0, 0x11450,
        0x114d0, 0x11650, 0x116c0, 0x116d0, 0x116da, 0x11730, 0x118e0, 0x11950, 0x11bf0, 0x11c50,
        0x11d50, 0x11da0, 0x11f50, 0x16130, 0x16a60, 0x16ac0, 0x16b50, 0x16d70, 0x1ccf0, 0x1d7ce,
        0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0, 0x1e5f1, 0x1e950, 0x1fbf0,
    ];
    let codepoint = u32::from(character);
    ZEROES.iter().find_map(|zero| {
        let difference = codepoint.checked_sub(*zero)?;
        (difference < 10).then_some(difference as u8)
    })
}

fn python_int_string(raw: &str) -> Option<i64> {
    let text = python_capability_trim(raw);
    let (negative, digits) = if let Some(digits) = text.strip_prefix('-') {
        (true, digits)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let mut value = 0i64;
    let mut saw_digit = false;
    let mut previous_was_digit = false;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        let digit = i64::from(unicode_decimal_digit(character)?);
        value = value
            .saturating_mul(10)
            .saturating_add(digit)
            .min(MAX_CONTEXT_WINDOW + 1);
        saw_digit = true;
        previous_was_digit = true;
    }
    if !saw_digit || !previous_was_digit {
        return None;
    }
    Some(if negative { -value } else { value })
}

fn python_u64_string(raw: &str) -> Option<(bool, u64)> {
    let text = python_capability_trim(raw);
    let (negative, digits) = if let Some(digits) = text.strip_prefix('-') {
        (true, digits)
    } else {
        (false, text.strip_prefix('+').unwrap_or(text))
    };
    let mut value = 0u64;
    let mut saw_digit = false;
    let mut previous_was_digit = false;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        let digit = u64::from(unicode_decimal_digit(character)?);
        value = value.checked_mul(10)?.checked_add(digit)?;
        saw_digit = true;
        previous_was_digit = true;
    }
    (saw_digit && previous_was_digit).then_some((negative, value))
}

pub(super) fn python_int(raw: &Value) -> Result<i64, ()> {
    match raw {
        Value::Bool(value) => Ok(i64::from(*value)),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(integer)
            } else if let Some(unsigned) = value.as_u64() {
                Ok(i64::try_from(unsigned).unwrap_or(MAX_CONTEXT_WINDOW + 1))
            } else {
                let number = value.as_f64().ok_or(())?;
                if !number.is_finite() {
                    return Err(());
                }
                let truncated = number.trunc();
                if truncated > MAX_CONTEXT_WINDOW as f64 {
                    Ok(MAX_CONTEXT_WINDOW + 1)
                } else if truncated < -(MAX_CONTEXT_WINDOW as f64) {
                    Ok(-(MAX_CONTEXT_WINDOW + 1))
                } else {
                    Ok(truncated as i64)
                }
            }
        }
        Value::String(value) => python_int_string(value).ok_or(()),
        _ => Err(()),
    }
}

pub(super) fn python_int_conversion_error(raw: &Value) -> ConfigError {
    match raw {
        Value::String(value) => ConfigError::python(
            "ValueError",
            format!(
                "invalid literal for int() with base 10: '{}'",
                value
                    .replace('\\', "\\\\")
                    .replace('\'', "\\'")
                    .replace('\n', "\\n")
                    .replace('\r', "\\r")
                    .replace('\t', "\\t")
            ),
        ),
        Value::Array(_) => ConfigError::python(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'list'",
        ),
        Value::Object(_) => ConfigError::python(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'",
        ),
        Value::Null => ConfigError::python(
            "TypeError",
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'",
        ),
        _ => ConfigError::python("ValueError", "invalid integer value"),
    }
}

pub(super) fn model_python_int_or_zero(raw: Option<&Value>) -> ConfigResult<i64> {
    match raw {
        None => Ok(0),
        Some(value) if !json_truthy(value) => Ok(0),
        Some(value) => python_int(value).map_err(|_| python_int_conversion_error(value)),
    }
}

pub(super) fn normalize_created_at(raw: Option<&Value>) -> ConfigResult<Value> {
    let Some(raw) = raw.filter(|value| json_truthy(value)) else {
        return Ok(Value::Number(0.into()));
    };
    let value = match raw {
        Value::Bool(true) => Value::Number(1.into()),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                if integer < 0 {
                    return Err(ConfigError::new("model.created_at cannot be negative"));
                }
                Value::Number(integer.into())
            } else if let Some(unsigned) = number.as_u64() {
                Value::Number(unsigned.into())
            } else {
                let number = number
                    .as_f64()
                    .ok_or_else(|| ConfigError::new("model.created_at must be numeric"))?;
                if !number.is_finite() || number > u64::MAX as f64 {
                    return Err(ConfigError::new("model.created_at is too large"));
                }
                let integer = number.trunc();
                if integer < 0.0 {
                    return Err(ConfigError::new("model.created_at cannot be negative"));
                }
                Value::Number((integer as u64).into())
            }
        }
        Value::String(value) => {
            let (negative, integer) =
                python_u64_string(value).ok_or_else(|| python_int_conversion_error(raw))?;
            if negative && integer != 0 {
                return Err(ConfigError::new("model.created_at cannot be negative"));
            }
            Value::Number(integer.into())
        }
        _ => return Err(python_int_conversion_error(raw)),
    };
    Ok(value)
}

fn python_float(raw: &Value) -> Result<f64, ()> {
    let value = match raw {
        Value::Bool(value) => f64::from(*value),
        Value::Number(value) => value.as_f64().ok_or(())?,
        Value::String(value) => {
            let text = python_capability_trim(value);
            let mut normalized = String::with_capacity(text.len());
            let mut previous_was_digit = false;
            let mut characters = text.chars().peekable();
            while let Some(character) = characters.next() {
                if character == '_' {
                    if !previous_was_digit
                        || characters
                            .peek()
                            .and_then(|next| unicode_decimal_digit(*next))
                            .is_none()
                    {
                        return Err(());
                    }
                    previous_was_digit = false;
                    continue;
                }
                if let Some(digit) = unicode_decimal_digit(character) {
                    normalized.push(char::from(b'0' + digit));
                    previous_was_digit = true;
                } else {
                    normalized.push(character);
                    previous_was_digit = false;
                }
            }
            let lowercase = normalized.to_ascii_lowercase();
            match lowercase.as_str() {
                "nan" | "+nan" | "-nan" => f64::NAN,
                "inf" | "+inf" | "infinity" | "+infinity" => f64::INFINITY,
                "-inf" | "-infinity" => f64::NEG_INFINITY,
                _ => normalized.parse::<f64>().map_err(|_| ())?,
            }
        }
        _ => return Err(()),
    };
    Ok(value)
}

fn calibration_confidence(raw: &Value, name: &str) -> ConfigResult<Value> {
    let value = python_float(raw).map_err(|_| {
        ConfigError::new(format!("model.context_calibrations.{name} must be numeric"))
    })?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(ConfigError::new(format!(
            "model.context_calibrations.{name} is out of range"
        )));
    }
    Ok(Value::Number(
        serde_json::Number::from_f64(value).expect("finite confidence"),
    ))
}

/// Normalize bounded model context calibration records exactly like Python.
///
/// Unknown fields are discarded, only the first eight entries are retained, and
/// numeric estimates preserve Python's permissive integer parsing. The result
/// has the normalized caller shape consumed by `_normalize_model`.
pub fn normalize_context_calibrations(raw: Option<&Value>) -> ConfigResult<Value> {
    let raw = match raw {
        None | Some(Value::Null) => return Ok(Value::Array(Vec::new())),
        Some(Value::Array(raw)) => raw,
        Some(_) => {
            return Err(ConfigError::new(
                "model.context_calibrations must be a list",
            ));
        }
    };
    let mut result = Vec::new();
    for item in raw.iter().take(CONTEXT_CALIBRATION_LIMIT) {
        let Value::Object(item) = item else {
            return Err(ConfigError::new(
                "model.context_calibrations entries must be objects",
            ));
        };
        let fingerprint = string_value(
            item.get("endpoint_fingerprint"),
            "model.context_calibrations.endpoint_fingerprint",
            false,
        )?;
        let valid_fingerprint = fingerprint.len() == 71
            && fingerprint.starts_with("sha256:")
            && fingerprint[7..]
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
        if !valid_fingerprint {
            return Err(ConfigError::new(
                "model.context_calibrations endpoint fingerprint is invalid",
            ));
        }
        let protocol = string_value(
            item.get("protocol"),
            "model.context_calibrations.protocol",
            false,
        )?;
        if !CONTEXT_CALIBRATION_PROTOCOLS.contains(&protocol.as_str()) {
            return Err(ConfigError::new(
                "model.context_calibrations protocol is invalid",
            ));
        }
        let upstream = safe_capability_identity(
            item.get("upstream_model"),
            "model.context_calibrations.upstream_model",
        )?;
        let deployment = safe_capability_identity(
            item.get("deployment_identity"),
            "model.context_calibrations.deployment_identity",
        )?;
        if upstream.is_empty() || deployment.is_empty() {
            return Err(ConfigError::new(
                "model.context_calibrations identity is required",
            ));
        }
        let mut clean = Map::new();
        clean.insert(
            "endpoint_fingerprint".to_owned(),
            Value::String(fingerprint),
        );
        clean.insert("upstream_model".to_owned(), Value::String(upstream));
        clean.insert("protocol".to_owned(), Value::String(protocol));
        clean.insert("deployment_identity".to_owned(), Value::String(deployment));
        for (name, value) in [
            (
                "largest_success_estimate",
                item.get("largest_success_estimate"),
            ),
            (
                "smallest_failure_estimate",
                item.get("smallest_failure_estimate"),
            ),
        ] {
            let is_empty = value.is_none()
                || matches!(value, Some(Value::Null))
                || matches!(value, Some(Value::String(value)) if value.is_empty());
            let normalized = if is_empty {
                Value::Null
            } else {
                let integer = python_int(value.expect("non-empty estimate")).map_err(|_| {
                    ConfigError::new(format!("model.context_calibrations.{name} must be numeric"))
                })?;
                if integer <= 0 || integer > MAX_CONTEXT_WINDOW {
                    return Err(ConfigError::new(format!(
                        "model.context_calibrations.{name} is out of range"
                    )));
                }
                Value::Number(serde_json::Number::from(integer))
            };
            clean.insert(name.to_owned(), normalized);
        }
        for name in ["largest_success_source", "smallest_failure_source"] {
            let source = match item.get(name) {
                None => "unknown".to_owned(),
                raw => string_value(raw, &format!("model.context_calibrations.{name}"), false)?,
            };
            if source != "observed" && source != "unknown" {
                return Err(ConfigError::new(format!(
                    "model.context_calibrations.{name} is invalid"
                )));
            }
            clean.insert(name.to_owned(), Value::String(source));
        }
        for name in ["largest_success_confidence", "smallest_failure_confidence"] {
            let estimate_name = if name == "largest_success_confidence" {
                "largest_success_estimate"
            } else {
                "smallest_failure_estimate"
            };
            let default = if clean
                .get(estimate_name)
                .is_some_and(|value| !value.is_null())
            {
                1.0
            } else {
                0.0
            };
            let default_confidence =
                Value::Number(serde_json::Number::from_f64(default).expect("default confidence"));
            let raw_confidence = item.get(name).unwrap_or(&default_confidence);
            clean.insert(
                name.to_owned(),
                calibration_confidence(raw_confidence, name)?,
            );
        }
        for name in [
            "largest_success_observed_at",
            "smallest_failure_observed_at",
        ] {
            let observed_at = match item.get(name) {
                None | Some(Value::Null) => Value::Null,
                Some(Value::String(value)) if valid_python_iso_timestamp(value) => {
                    Value::String(value.clone())
                }
                Some(_) => {
                    return Err(ConfigError::new(format!(
                        "model.context_calibrations.{name} is invalid"
                    )));
                }
            };
            clean.insert(name.to_owned(), observed_at);
        }
        result.push(Value::Object(clean));
    }
    Ok(Value::Array(result))
}
