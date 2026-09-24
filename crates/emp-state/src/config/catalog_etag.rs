//! Python-compatible catalog JSON bytes and ETag.

use super::*;
use sha2::{Digest, Sha256};

fn python_exponent(mantissa: &str, exponent: i32) -> String {
    format!("{mantissa}e{exponent:+03}")
}

fn python_float_token(rendered: &str) -> String {
    if let Some((mantissa, exponent)) = rendered.split_once('e') {
        if let Ok(exponent) = exponent.parse::<i32>() {
            return python_exponent(mantissa, exponent);
        }
        return rendered.to_owned();
    }

    let (sign, unsigned) = rendered
        .strip_prefix('-')
        .map_or(("", rendered), |value| ("-", value));
    let Some(fraction) = unsigned.strip_prefix("0.") else {
        return rendered.to_owned();
    };
    let Some(first_nonzero) = fraction.bytes().position(|value| value != b'0') else {
        return rendered.to_owned();
    };
    let exponent = -(first_nonzero as i32) - 1;
    if exponent >= -4 {
        return rendered.to_owned();
    }
    let digits = &fraction[first_nonzero..];
    let mantissa = if digits.len() == 1 {
        format!("{sign}{digits}")
    } else {
        format!("{sign}{}.{}", &digits[..1], &digits[1..])
    };
    python_exponent(&mantissa, exponent)
}

fn write_python_json(value: &Value, output: &mut Vec<u8>) -> ConfigResult<()> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => {
            let rendered = number.to_string();
            if number.is_f64() {
                output.extend_from_slice(python_float_token(&rendered).as_bytes());
            } else {
                output.extend_from_slice(rendered.as_bytes());
            }
        }
        Value::String(value) => serde_json::to_writer(output, value)
            .map_err(|_| ConfigError::new("catalog cannot be canonically encoded"))?,
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_python_json(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(entries) => {
            output.push(b'{');
            let mut sorted = entries.iter().collect::<Vec<_>>();
            sorted.sort_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .map_err(|_| ConfigError::new("catalog cannot be canonically encoded"))?;
                output.push(b':');
                write_python_json(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

/// Return the exact compact UTF-8 JSON bytes hashed by Python's catalog ETag.
pub fn canonical_catalog_json(value: &Value) -> ConfigResult<Vec<u8>> {
    let mut output = Vec::new();
    write_python_json(value, &mut output)?;
    Ok(output)
}

/// Return the Python-compatible quoted catalog ETag.
pub fn catalog_etag(catalog: &Value) -> ConfigResult<String> {
    let digest = Sha256::digest(canonical_catalog_json(catalog)?);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(format!("\"emp-{encoded}\""))
}
