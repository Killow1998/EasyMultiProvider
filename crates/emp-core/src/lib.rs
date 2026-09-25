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

mod observation;
mod route;
#[cfg(test)]
mod tests;
mod validation;

pub use observation::{PublicFailure, StreamObservation};
pub use validation::{OpaqueJson, OpaqueJsonError, OpaqueJsonErrorReason, ValidationError};

pub use route::{
    RouteResolutionError, classify_dialect, deployment_identity, endpoint_fingerprint,
    normalize_endpoint, resolve_route, resolve_route_without_catalog, resolved_route_from_parts,
    resolved_upstream_model,
};

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

/// A configured protocol identity.  `Auto` is retained because route
/// resolution is an observation over configuration; concrete protocol
/// selection happens later and may attempt multiple transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    Auto,
    Responses,
    ChatCompletions,
    AnthropicMessages,
}

impl Protocol {
    pub const fn as_config_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Responses => "responses",
            Self::ChatCompletions => "chat_completions",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }
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

pub mod capability_view;
