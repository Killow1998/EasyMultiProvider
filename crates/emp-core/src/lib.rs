//! Shared foundation types for the Rust replacement of EasyMultiProvider.
//!
//! These types are deliberately narrow.  They preserve unknown provider and
//! protocol JSON while giving the router, transport, and application crates a
//! stable vocabulary for request preparation and terminal observations.  This
//! is the beginning of the Rust port, not a complete behavior port.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

/// Route selector provenance retained from the Python resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteSource {
    ExplicitModel,
    SubscriptionAccount,
    ForwardProvider,
    ImplicitNative,
}

/// Request dialect classification, independent of the concrete HTTP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dialect {
    CodexNative,
    PortableResponses,
    ChatCompletions,
    AnthropicMessages,
}

/// A concrete upstream protocol selected for one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Responses,
    ChatCompletions,
    AnthropicMessages,
}

/// HTTP method retained for future prepared-request checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    Get,
    Post,
}

fn is_sha256_fingerprint(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_token(value: &str) -> String {
    let normalized: String = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    normalized.trim_end_matches('_').to_string()
}

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

fn valid_deployment_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b':' | b'-')
        })
}

/// Immutable route snapshot produced after configuration resolution.
///
/// Provider and model snapshots are opaque so unknown configuration fields are
/// preserved.  Request-local changes must clone those snapshots rather than
/// mutating the route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedRoute {
    pub requested_model: String,
    pub upstream_model: String,
    pub source: RouteSource,
    pub provider: OpaqueJson,
    pub model: OpaqueJson,
    pub protocol: Protocol,
    pub dialect: Dialect,
    pub provider_id: String,
    pub endpoint_fingerprint: String,
    pub deployment_identity: String,
}

impl ResolvedRoute {
    /// Construct and validate a route.  Fingerprints and identity fields are
    /// checked so later components cannot silently lose their provenance.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        requested_model: impl Into<String>,
        upstream_model: impl Into<String>,
        source: RouteSource,
        provider: Map<String, Value>,
        model: Map<String, Value>,
        protocol: Protocol,
        dialect: Dialect,
        provider_id: impl Into<String>,
        endpoint_fingerprint: impl Into<String>,
        deployment_identity: impl Into<String>,
    ) -> Result<Self, ValidationError> {
        let requested_model = requested_model.into();
        let upstream_model = upstream_model.into();
        let provider_id = provider_id.into();
        let endpoint_fingerprint = endpoint_fingerprint.into();
        let deployment_identity = deployment_identity.into();
        if requested_model.is_empty() {
            return Err(ValidationError::EmptyField("requested_model"));
        }
        if upstream_model.is_empty() {
            return Err(ValidationError::EmptyField("upstream_model"));
        }
        if provider_id.is_empty() {
            return Err(ValidationError::EmptyField("provider_id"));
        }
        if !is_sha256_fingerprint(&endpoint_fingerprint) {
            return Err(ValidationError::InvalidFingerprint);
        }
        if !valid_deployment_identity(&deployment_identity) {
            return Err(ValidationError::InvalidDeploymentIdentity);
        }
        let route = Self {
            requested_model,
            upstream_model,
            source,
            provider: OpaqueJson::new(provider)?,
            model: OpaqueJson::new(model)?,
            protocol,
            dialect,
            provider_id,
            endpoint_fingerprint,
            deployment_identity,
        };
        Ok(route)
    }
}

/// Transport admission limits carried with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestLimits {
    pub max_wire_bytes: usize,
    pub max_decoded_bytes: usize,
}

impl RequestLimits {
    pub fn new(max_wire_bytes: usize, max_decoded_bytes: usize) -> Self {
        Self {
            max_wire_bytes,
            max_decoded_bytes,
        }
    }
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self::new(Self::DEFAULT_WIRE_BYTES, Self::DEFAULT_DECODED_BYTES)
    }
}

impl RequestLimits {
    pub const DEFAULT_WIRE_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_DECODED_BYTES: usize = 64 * 1024 * 1024;
}

/// Request-local facts supplied by the application boundary.
///
/// The context does not own configuration.  It retains only the safe identity,
/// timing, admission, and metadata needed by later preparation and transport
/// components.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestContext {
    pub request_id: String,
    pub received_at: Duration,
    pub deadline_at: Option<Duration>,
    pub stream: bool,
    #[serde(default)]
    pub incoming_headers: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: RequestLimits,
    #[serde(default)]
    pub metadata: OpaqueJson,
}

impl RequestContext {
    /// Construct a context with default limits.  Python uses a 16-character
    /// lowercase request id; preserving that shape avoids introducing a second
    /// diagnostic identifier format.
    pub fn new(
        request_id: impl Into<String>,
        received_at: Duration,
        stream: bool,
    ) -> Result<Self, ValidationError> {
        let request_id = request_id.into();
        let valid = request_id.len() == 16
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        if !valid {
            return Err(ValidationError::InvalidRequestId);
        }
        let metadata = OpaqueJson::new(Map::new())?;
        Ok(Self {
            request_id,
            received_at,
            deadline_at: None,
            stream,
            incoming_headers: BTreeMap::new(),
            limits: RequestLimits::default(),
            metadata,
        })
    }

    /// Set a monotonic deadline.  Zero-duration requests are accepted, but a
    /// deadline before receipt is inconsistent and rejected.
    pub fn with_deadline(mut self, deadline_at: Duration) -> Result<Self, ValidationError> {
        if deadline_at < self.received_at {
            return Err(ValidationError::InvalidDeadline);
        }
        self.deadline_at = Some(deadline_at);
        Ok(self)
    }
}

/// A translated, encoded-ready request with its immutable route snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreparedRequest {
    pub route: ResolvedRoute,
    pub method: Method,
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: Option<OpaqueJson>,
    #[serde(default)]
    pub metadata: OpaqueJson,
}

impl PreparedRequest {
    /// Construct and validate one prepared request.
    pub fn new(
        route: ResolvedRoute,
        method: Method,
        url: impl Into<String>,
        headers: BTreeMap<String, String>,
        body: Option<Map<String, Value>>,
        metadata: Map<String, Value>,
    ) -> Result<Self, ValidationError> {
        let url = url.into();
        let valid_scheme = url
            .split_once(':')
            .map(|(scheme, rest)| {
                matches!(
                    scheme.to_ascii_lowercase().as_str(),
                    "http" | "https" | "ws" | "wss"
                ) && !rest.is_empty()
            })
            .unwrap_or(false);
        if !valid_scheme {
            return Err(ValidationError::InvalidUrl);
        }
        let body = body.map(OpaqueJson::new).transpose()?;
        let metadata = OpaqueJson::new(metadata)?;
        Ok(Self {
            route,
            method,
            url,
            headers,
            body,
            metadata,
        })
    }
}

/// A content-free public failure safe for clients and diagnostics.
///
/// `status` is optional because a client disconnect may not have an HTTP
/// response boundary.  Unknown context observations remain opaque.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicFailure {
    pub error_class: String,
    pub status: Option<u16>,
    pub phase: Option<String>,
    pub failure_reason: Option<String>,
    pub public_message: Option<String>,
    pub retry_after_seconds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_observation: Option<OpaqueJson>,
}

impl PublicFailure {
    /// Construct a normalized failure.  Failure reasons use the existing
    /// 64-character safe token policy; messages remain bounded but unchanged.
    pub fn new(
        error_class: impl Into<String>,
        status: Option<u16>,
        phase: Option<String>,
        failure_reason: Option<String>,
        public_message: Option<String>,
        retry_after_seconds: Option<u32>,
        context_observation: Option<Map<String, Value>>,
    ) -> Result<Self, ValidationError> {
        let error_class = safe_token(&error_class.into());
        if error_class.is_empty() {
            return Err(ValidationError::EmptyField("error_class"));
        }
        if status.is_some_and(|value| !(100..=599).contains(&value)) {
            return Err(ValidationError::InvalidStatus);
        }
        let failure_reason = failure_reason
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(safe_token)
            .filter(|value| !value.is_empty());
        let public_message = public_message
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.chars().take(512).collect());
        let phase = phase
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(safe_token)
            .filter(|value| !value.is_empty());
        let context_observation = context_observation.map(OpaqueJson::new).transpose()?;
        Ok(Self {
            error_class,
            status,
            phase,
            failure_reason,
            public_message,
            retry_after_seconds,
            context_observation,
        })
    }
}

/// Terminal stream facts shared by SSE and WebSocket adapters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamObservation {
    pub phase: String,
    pub duration_ms: u64,
    pub first_event_ms: Option<u64>,
    pub retry_count: u32,
    pub output_emitted: bool,
    pub tool_activity: bool,
    pub upstream_event_observed: bool,
    pub terminal_event_observed: bool,
    pub terminal: Option<PublicFailure>,
    #[serde(default)]
    pub unknown_fields: OpaqueJson,
}

impl StreamObservation {
    /// Construct an observation.  A terminal event must have a classified
    /// terminal value so stream truth cannot be silently discarded.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        phase: impl Into<String>,
        duration_ms: u64,
        first_event_ms: Option<u64>,
        retry_count: u32,
        output_emitted: bool,
        tool_activity: bool,
        upstream_event_observed: bool,
        terminal_event_observed: bool,
        terminal: Option<PublicFailure>,
        unknown_fields: Map<String, Value>,
    ) -> Result<Self, ValidationError> {
        if terminal_event_observed && terminal.is_none() {
            return Err(ValidationError::EmptyField("terminal"));
        }
        let phase = phase.into();
        if phase.is_empty() {
            return Err(ValidationError::EmptyField("phase"));
        }
        Ok(Self {
            phase,
            duration_ms,
            first_event_ms,
            retry_count,
            output_emitted,
            tool_activity,
            upstream_event_observed,
            terminal_event_observed,
            terminal,
            unknown_fields: OpaqueJson::new(unknown_fields)?,
        })
    }
}

/// Injectable wall/monotonic clock used for deterministic policy tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;

    fn deadline_after(&self, timeout: Duration) -> Duration {
        self.now().saturating_add(timeout)
    }
}

/// Injectable randomness used for request and generated identifiers.
pub trait RandomSource: Send + Sync {
    fn fill(&self, destination: &mut [u8]);

    fn request_id(&self) -> String {
        let mut bytes = [0_u8; 8];
        self.fill(&mut bytes);
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

/// One bounded memory admission result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryAdmission {
    pub required_bytes: u64,
    pub available_bytes: Option<u64>,
}

/// A bounded memory refusal without host process details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryRefusal {
    pub required_bytes: u64,
    pub available_bytes: Option<u64>,
    pub reason: String,
}

/// Injectable memory-inspection boundary.
///
/// Concrete process inspection belongs behind this trait so request admission
/// can be tested deterministically and cannot accidentally depend on Python.
pub trait MemoryInspector: Send + Sync {
    fn admit(&self, required_bytes: u64) -> Result<MemoryAdmission, MemoryRefusal>;
}

/// Deterministic clock for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedClock(pub Duration);

impl Clock for FixedClock {
    fn now(&self) -> Duration {
        self.0
    }
}

/// Deterministic random source for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FixedRandomSource(pub u8);

impl RandomSource for FixedRandomSource {
    fn fill(&self, destination: &mut [u8]) {
        destination.fill(self.0);
    }
}

/// Memory inspector that accepts every request without claiming real process
/// memory.  Production must inject a validated platform implementation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PermissiveMemoryInspector;

impl MemoryInspector for PermissiveMemoryInspector {
    fn admit(&self, required_bytes: u64) -> Result<MemoryAdmission, MemoryRefusal> {
        Ok(MemoryAdmission {
            required_bytes,
            available_bytes: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn provider_object() -> Map<String, Value> {
        let value = json!({
            "id": "demo",
            "base_url": "https://example.test/v1",
            "protocol": "responses",
            "future_provider_field": {"unknown": true}
        });
        value.as_object().expect("object").clone()
    }

    fn model_object() -> Map<String, Value> {
        let value = json!({
            "id": "demo/model",
            "upstream_id": "model",
            "future_model_field": [1, 2, 3]
        });
        value.as_object().expect("object").clone()
    }

    fn route() -> ResolvedRoute {
        ResolvedRoute::new(
            "demo/model",
            "model",
            RouteSource::ExplicitModel,
            provider_object(),
            model_object(),
            Protocol::Responses,
            Dialect::PortableResponses,
            "demo",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            "default",
        )
        .expect("valid route")
    }

    #[test]
    fn opaque_json_preserves_unknown_fields() {
        let raw = json!({"type": "response.create", "future": {"nested": [1, null, "x"]}});
        let value = raw.as_object().expect("object").clone();
        let opaque = OpaqueJson::new(value).expect("valid");
        let restored = serde_json::to_value(&opaque).expect("serialize");
        assert_eq!(restored, raw);
    }

    #[test]
    fn opaque_json_enforces_its_limit() {
        let value = json!({"large": "0123456789"});
        let error = OpaqueJson::with_limit(value.as_object().expect("object").clone(), 8)
            .expect_err("too large");
        assert_eq!(error.reason, OpaqueJsonErrorReason::TooLarge);
    }

    #[test]
    fn resolved_route_round_trips_unknown_snapshots() {
        let route = route();
        let serialized = serde_json::to_string(&route).expect("serialize");
        assert!(serialized.contains("\"future_provider_field\":{\"unknown\":true}"));
        let restored: ResolvedRoute = serde_json::from_str(&serialized).expect("deserialize");
        assert_eq!(restored, route);
        assert_eq!(
            restored.provider.value().get("future_provider_field"),
            Some(&json!({"unknown": true}))
        );
        assert_eq!(
            restored.model.value().get("future_model_field"),
            Some(&json!([1, 2, 3]))
        );
        assert_eq!(restored.source, RouteSource::ExplicitModel);
        assert_eq!(restored.dialect, Dialect::PortableResponses);
    }

    #[test]
    fn opaque_json_deserialization_rejects_non_objects_and_preserves_fields() {
        assert!(serde_json::from_str::<OpaqueJson>("[]").is_err());
        assert!(serde_json::from_str::<OpaqueJson>("42").is_err());
        let valid: OpaqueJson =
            serde_json::from_str(r#"{"future":{"unknown":true}}"#).expect("valid JSON");
        assert_eq!(valid.value().get("future"), Some(&json!({"unknown": true})));
    }

    #[test]
    fn opaque_json_with_limit_rejects_oversized_deserialized_values() {
        let raw = r#"{"future":{"values":"0123456789"}}"#;
        let value: Map<String, Value> = serde_json::from_str(raw).expect("parse raw");
        let error = OpaqueJson::with_limit(value, 8)
            .err()
            .map(|error| error.to_string())
            .expect("oversized opaque JSON should fail");
        assert!(error.contains("limit is 8 bytes"), "{error}");
    }

    #[test]
    fn resolved_route_rejects_invalid_fingerprint() {
        let error = ResolvedRoute::new(
            "demo/model",
            "model",
            RouteSource::ExplicitModel,
            provider_object(),
            model_object(),
            Protocol::Responses,
            Dialect::PortableResponses,
            "demo",
            "endpoint",
            "default",
        )
        .expect_err("invalid fingerprint");
        assert_eq!(error, ValidationError::InvalidFingerprint);
    }

    #[test]
    fn request_context_enforces_identity_and_deadline() {
        let context = RequestContext::new("0123456789abcdef", Duration::from_secs(10), true)
            .expect("valid context")
            .with_deadline(Duration::from_secs(20))
            .expect("valid deadline");
        assert!(context.stream);
        assert_eq!(context.deadline_at, Some(Duration::from_secs(20)));

        let short_id = RequestContext::new("short", Duration::ZERO, false).expect_err("invalid id");
        assert_eq!(short_id, ValidationError::InvalidRequestId);
        let backwards = RequestContext::new("0123456789abcdef", Duration::from_secs(20), false)
            .expect("valid context")
            .with_deadline(Duration::from_secs(10))
            .expect_err("invalid deadline");
        assert_eq!(backwards, ValidationError::InvalidDeadline);
    }

    #[test]
    fn prepared_request_preserves_unknown_body_fields() {
        let route = route();
        let body = json!({
            "model": "demo/model",
            "input": "continue",
            "future_protocol_field": {"opaque": true}
        });
        let headers = BTreeMap::from([("authorization".to_string(), "Bearer fixture".to_string())]);
        let request = PreparedRequest::new(
            route.clone(),
            Method::Post,
            "https://example.test/v1/responses",
            headers,
            Some(body.as_object().expect("object").clone()),
            Map::new(),
        )
        .expect("valid request");
        assert_eq!(
            request
                .body
                .as_ref()
                .expect("body")
                .value()
                .get("future_protocol_field"),
            Some(&json!({"opaque": true}))
        );
        let restored: PreparedRequest =
            serde_json::from_str(&serde_json::to_string(&request).expect("serialize"))
                .expect("deserialize");
        assert_eq!(restored, request);
    }

    #[test]
    fn prepared_request_requires_supported_url() {
        let error = PreparedRequest::new(
            route(),
            Method::Post,
            "gopher://example.test",
            BTreeMap::new(),
            None,
            Map::new(),
        )
        .expect_err("invalid URL");
        assert_eq!(error, ValidationError::InvalidUrl);
    }

    #[test]
    fn public_failure_normalizes_safe_tokens() {
        let failure = PublicFailure::new(
            "Rate Limit",
            Some(429),
            Some("First Event".to_string()),
            Some("upstream capacity!!".to_string()),
            Some("The upstream rate limit was reached.".to_string()),
            Some(15),
            None,
        )
        .expect("valid failure");
        assert_eq!(failure.error_class, "rate_limit");
        assert_eq!(failure.phase.as_deref(), Some("first_event"));
        assert_eq!(failure.failure_reason.as_deref(), Some("upstream_capacity"));
        let serialized = serde_json::to_value(&failure).expect("serialize");
        assert_eq!(serialized["status"], 429);
        assert!(serialized.get("context_observation").is_none());

        let invalid = PublicFailure::new("none", Some(99), None, None, None, None, None)
            .expect_err("invalid status");
        assert_eq!(invalid, ValidationError::InvalidStatus);
    }

    #[test]
    fn stream_observation_requires_terminal_truth() {
        let terminal = PublicFailure::new(
            "stream_incomplete",
            Some(502),
            Some("terminal_validation".to_string()),
            Some("stream incomplete".to_string()),
            Some("The upstream stream ended without a valid completion event.".to_string()),
            None,
            None,
        )
        .expect("terminal");
        let observation = StreamObservation::new(
            "terminal_validation",
            125,
            Some(20),
            0,
            true,
            false,
            true,
            true,
            Some(terminal.clone()),
            Map::new(),
        )
        .expect("observation");
        assert_eq!(observation.terminal, Some(terminal));
        let missing = StreamObservation::new(
            "terminal_validation",
            125,
            None,
            0,
            false,
            false,
            true,
            true,
            None,
            Map::new(),
        )
        .expect_err("terminal missing");
        assert_eq!(missing, ValidationError::EmptyField("terminal"));
    }

    #[test]
    fn injected_sources_are_deterministic() {
        let clock = FixedClock(Duration::from_secs(42));
        assert_eq!(clock.now(), Duration::from_secs(42));
        assert_eq!(
            clock.deadline_after(Duration::from_secs(1)),
            Duration::from_secs(43)
        );

        let random = FixedRandomSource(0xa5);
        assert_eq!(random.request_id(), "a5a5a5a5a5a5a5a5");
        let admission = PermissiveMemoryInspector
            .admit(1024)
            .expect("permissive admission");
        assert_eq!(admission.required_bytes, 1024);
        assert_eq!(admission.available_bytes, None);
    }
}
