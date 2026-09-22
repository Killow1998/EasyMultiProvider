//! Util.

use crate::error::AppError;
use emp_router::ProjectionIds;
use serde_json::Value;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

pub(crate) fn system_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn random_hex(bytes: usize) -> Result<String, AppError> {
    let mut raw = vec![0_u8; bytes];
    getrandom::getrandom(&mut raw).map_err(|_| AppError::RandomUnavailable)?;
    let mut encoded = String::with_capacity(bytes * 2);
    for byte in raw {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    Ok(encoded)
}

pub(crate) fn projection_ids() -> Result<ProjectionIds, AppError> {
    Ok(ProjectionIds::new(
        format!("resp_{}", random_hex(16)?),
        format!("msg_{}", random_hex(16)?),
        format!("rs_{}", random_hex(16)?),
        format!("rs_{}", random_hex(16)?),
    ))
}

pub(crate) fn python_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64() != Some(0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}
