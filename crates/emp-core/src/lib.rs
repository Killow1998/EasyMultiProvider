//! Shared foundation types for the Rust replacement of EasyMultiProvider.
//!
//! Immutable route identities and bounded provider/model snapshots shared by
//! routing, protocol projection and catalog presentation.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

mod route;
#[cfg(test)]
mod tests;
mod validation;

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

pub mod capability_view;
