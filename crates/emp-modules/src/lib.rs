pub mod handshake;
pub mod legacy_v2;
pub mod manifest;
pub mod peer;
mod strict_json;

pub use legacy_v2::*;

pub fn reply_object(
    value: serde_json::Value,
) -> serde_json::Result<serde_json::Map<String, serde_json::Value>> {
    match value {
        serde_json::Value::Object(fields) => Ok(fields),
        _ => Err(serde::de::Error::custom(
            "module reply must be a JSON object",
        )),
    }
}
