use super::*;

/// A bounded opaque JSON object.
///
/// Unknown native protocol fields must survive a Rust round trip.  The size
/// bound keeps an admission decision deterministic without interpreting any
/// field as a final protocol type.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct OpaqueJson {
    value: Map<String, Value>,
}

impl<'de> Deserialize<'de> for OpaqueJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Map::<String, Value>::deserialize(deserializer)?;
        OpaqueJson::new(value).map_err(serde::de::Error::custom)
    }
}

impl OpaqueJson {
    /// Current request-body admission bound until transport policies move to
    /// this crate.
    pub const MAX_BYTES: usize = 64 * 1024 * 1024;

    /// Validate and retain an opaque JSON object.
    pub fn new(value: Map<String, Value>) -> Result<Self, OpaqueJsonError> {
        Self::with_limit(value, Self::MAX_BYTES)
    }

    /// Validate against a non-default limit.  Persistence and transport always
    /// use [`Self::MAX_BYTES`], so the retained object has no per-instance
    /// policy that can bypass validation after deserialization.
    pub fn with_limit(
        value: Map<String, Value>,
        max_bytes: usize,
    ) -> Result<Self, OpaqueJsonError> {
        let result = Self { value };
        result.validate(max_bytes)?;
        Ok(result)
    }

    pub fn value(&self) -> &Map<String, Value> {
        &self.value
    }

    pub fn into_value(self) -> Map<String, Value> {
        self.value
    }

    fn validate(&self, max_bytes: usize) -> Result<(), OpaqueJsonError> {
        let encoded = serde_json::to_vec(&self.value).map_err(|error| OpaqueJsonError {
            reason: OpaqueJsonErrorReason::Serialization,
            message: error.to_string(),
        })?;
        if encoded.len() > max_bytes {
            return Err(OpaqueJsonError {
                reason: OpaqueJsonErrorReason::TooLarge,
                message: format!(
                    "opaque JSON is {} bytes; limit is {} bytes",
                    encoded.len(),
                    max_bytes
                ),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueJsonErrorReason {
    TooLarge,
    Serialization,
}

/// A bounded serialization failure that never includes JSON content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueJsonError {
    pub reason: OpaqueJsonErrorReason,
    message: String,
}

impl fmt::Display for OpaqueJsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for OpaqueJsonError {}

/// Validation failures for shared route and request records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    EmptyField(&'static str),
    InvalidFingerprint,
    InvalidDeploymentIdentity,
    InvalidRequestId,
    InvalidDeadline,
    InvalidUrl,
    InvalidStatus,
    OpaqueJson(OpaqueJsonError),
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyField(name) => write!(f, "required field is empty: {name}"),
            Self::InvalidFingerprint => f.write_str("endpoint fingerprint must be sha256:<64 hex>"),
            Self::InvalidDeploymentIdentity => {
                f.write_str("deployment identity contains unsupported characters")
            }
            Self::InvalidRequestId => f.write_str("request id must be 16 lowercase hex characters"),
            Self::InvalidDeadline => f.write_str("request deadline precedes received time"),
            Self::InvalidUrl => {
                f.write_str("prepared request URL must use http, https, ws, or wss")
            }
            Self::InvalidStatus => f.write_str("public failure status must be 100 through 599"),
            Self::OpaqueJson(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ValidationError {}

impl From<OpaqueJsonError> for ValidationError {
    fn from(value: OpaqueJsonError) -> Self {
        Self::OpaqueJson(value)
    }
}
