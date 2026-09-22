use crate::{Dialect, Protocol, ResolvedRoute, RouteSource, ValidationError};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use url::{Host, Url};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// A configuration-resolution failure with the same public status and message
/// boundary as Python's `RouterError` for user-selectable models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteResolutionError {
    ProviderUnavailable(String),
    UnknownModel(String),
    UnsupportedProtocol,
    InvalidRoute(ValidationError),
}

impl RouteResolutionError {
    pub const fn status(&self) -> u16 {
        match self {
            Self::UnknownModel(_) => 404,
            Self::ProviderUnavailable(_) | Self::UnsupportedProtocol | Self::InvalidRoute(_) => 503,
        }
    }
}

impl fmt::Display for RouteResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProviderUnavailable(model) => write!(
                formatter,
                "provider for model is missing or disabled: {model}"
            ),
            Self::UnknownModel(model) => write!(formatter, "unknown model: {model}"),
            Self::UnsupportedProtocol => formatter.write_str("provider protocol is unsupported"),
            Self::InvalidRoute(error) => write!(formatter, "invalid resolved route: {error}"),
        }
    }
}

impl std::error::Error for RouteResolutionError {}

impl From<ValidationError> for RouteResolutionError {
    fn from(value: ValidationError) -> Self {
        Self::InvalidRoute(value)
    }
}

/// Canonicalize an HTTP endpoint without retaining credentials, query data or
/// fragments. The raw path is normalized lexically so Unicode path bytes hash
/// exactly like Python's `urlsplit` plus `posixpath.normpath` implementation.
pub fn normalize_endpoint(endpoint: Option<&str>) -> String {
    let Some(input) = endpoint.map(str::trim).filter(|value| !value.is_empty()) else {
        return String::new();
    };
    let Ok(parsed) = Url::parse(input) else {
        return String::new();
    };
    let scheme = parsed.scheme().to_ascii_lowercase();
    if !matches!(scheme.as_str(), "http" | "https") {
        return String::new();
    }
    let Some(host) = parsed.host() else {
        return String::new();
    };
    let host = match host {
        Host::Domain(domain) => domain.to_ascii_lowercase(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
    };
    let default_port = if scheme == "http" { 80 } else { 443 };
    let port = parsed
        .port()
        .filter(|port| *port != default_port)
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    let path = normalize_posix_path(raw_path(input).unwrap_or("/"));
    format!("{scheme}://{host}{port}{path}")
}

fn raw_path(input: &str) -> Option<&str> {
    let (_, rest) = input.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if rest.as_bytes().get(authority_end) != Some(&b'/') {
        return Some("/");
    }
    let path_and_tail = &rest[authority_end..];
    let path_end = path_and_tail
        .find(['?', '#'])
        .unwrap_or(path_and_tail.len());
    Some(&path_and_tail[..path_end])
}

fn normalize_posix_path(path: &str) -> String {
    let mut segments = Vec::new();
    for segment in path.trim_start_matches('/').split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            value => segments.push(value),
        }
    }
    if segments.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// Hash only the canonical public endpoint identity.
pub fn endpoint_fingerprint(endpoint: Option<&str>) -> String {
    let canonical = normalize_endpoint(endpoint);
    let canonical = if canonical.is_empty() {
        "invalid-endpoint"
    } else {
        &canonical
    };
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

/// Select the first safe deployment identity, preferring model metadata.
pub fn deployment_identity(provider: &Map<String, Value>, model: &Map<String, Value>) -> String {
    for source in [model, provider] {
        for field in ["deployment_identity", "deployment_id", "deployment"] {
            let Some(value) = source.get(field).and_then(Value::as_str).map(str::trim) else {
                continue;
            };
            if !value.is_empty()
                && value.len() <= 256
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'/' | b':' | b'-')
                })
            {
                return value.to_owned();
            }
        }
    }
    "default".to_owned()
}

pub fn classify_dialect(provider: &Map<String, Value>) -> Dialect {
    match provider.get("protocol").and_then(Value::as_str) {
        Some("responses")
            if matches!(
                provider.get("auth_mode").and_then(Value::as_str),
                Some("account" | "forward")
            ) =>
        {
            Dialect::CodexNative
        }
        Some("responses") => Dialect::PortableResponses,
        Some("anthropic_messages") => Dialect::AnthropicMessages,
        _ => Dialect::ChatCompletions,
    }
}

fn protocol(provider: &Map<String, Value>) -> Result<Protocol, RouteResolutionError> {
    match provider.get("protocol").and_then(Value::as_str) {
        Some("auto") => Ok(Protocol::Auto),
        Some("responses") => Ok(Protocol::Responses),
        Some("chat_completions") => Ok(Protocol::ChatCompletions),
        Some("anthropic_messages") => Ok(Protocol::AnthropicMessages),
        _ => Err(RouteResolutionError::UnsupportedProtocol),
    }
}

/// Resolve the upstream identifier exactly once from a frozen provider/model
/// snapshot.
pub fn resolved_upstream_model(
    provider: &Map<String, Value>,
    model: &Map<String, Value>,
    requested: &str,
) -> String {
    let prefix = provider
        .get("id")
        .and_then(Value::as_str)
        .map(|id| format!("{id}/"))
        .unwrap_or_else(|| "/".to_owned());
    if let Some(explicit) = model
        .get("upstream_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return explicit
            .strip_prefix(&prefix)
            .filter(|_| prefix != "/")
            .unwrap_or(explicit)
            .to_owned();
    }
    requested
        .strip_prefix(&prefix)
        .filter(|_| prefix != "/")
        .unwrap_or(requested)
        .to_owned()
}

pub fn resolved_route_from_parts(
    requested_model: &str,
    provider: Map<String, Value>,
    model: Map<String, Value>,
    source: RouteSource,
) -> Result<ResolvedRoute, RouteResolutionError> {
    let upstream_model = resolved_upstream_model(&provider, &model, requested_model);
    let protocol = protocol(&provider)?;
    let dialect = classify_dialect(&provider);
    let provider_id = provider
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let fingerprint = endpoint_fingerprint(provider.get("base_url").and_then(Value::as_str));
    let deployment = deployment_identity(&provider, &model);
    Ok(ResolvedRoute::new(
        requested_model,
        upstream_model,
        source,
        provider,
        model,
        protocol,
        dialect,
        provider_id,
        fingerprint,
        deployment,
    )?)
}

impl ResolvedRoute {
    /// Return a new immutable route with a concrete negotiated protocol.
    pub fn with_protocol(&self, protocol: Protocol) -> Result<Self, RouteResolutionError> {
        if self.protocol == protocol {
            return Ok(self.clone());
        }
        let mut provider = self.provider.value().clone();
        provider.insert(
            "protocol".to_owned(),
            Value::String(protocol.as_config_str().to_owned()),
        );
        resolved_route_from_parts(
            &self.requested_model,
            provider,
            self.model.value().clone(),
            self.source,
        )
    }
}

/// Resolve an immutable request route from normalized configuration. Native
/// catalog access is injected so `emp-core` remains free of filesystem and
/// credential ownership.
pub fn resolve_route<F>(
    config: &Value,
    model_id: &str,
    mut subscription_model: F,
) -> Result<ResolvedRoute, RouteResolutionError>
where
    F: FnMut(&Map<String, Value>, &str, Option<&Map<String, Value>>) -> Option<Map<String, Value>>,
{
    let empty = Map::new();
    let config = config.as_object().unwrap_or(&empty);
    let models = config
        .get("models")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let providers = config
        .get("providers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    for model in models.iter().filter_map(Value::as_object) {
        if enabled(model) && model.get("id").and_then(Value::as_str) == Some(model_id) {
            let selected = model.get("provider").and_then(Value::as_str);
            let provider = providers
                .iter()
                .filter_map(Value::as_object)
                .find(|provider| provider.get("id").and_then(Value::as_str) == selected);
            if let Some(provider) = provider.filter(|provider| enabled(provider)) {
                return resolved_route_from_parts(
                    model_id,
                    provider.clone(),
                    model.clone(),
                    RouteSource::ExplicitModel,
                );
            }
            return Err(RouteResolutionError::ProviderUnavailable(
                model_id.to_owned(),
            ));
        }
    }

    let accounts = config
        .get("accounts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for account in accounts.iter().filter_map(Value::as_object) {
        let prefix = account
            .get("prefix")
            .and_then(Value::as_str)
            .map(|prefix| format!("{prefix}/"))
            .unwrap_or_else(|| "/".to_owned());
        if !enabled(account) || prefix == "/" || !model_id.starts_with(&prefix) {
            continue;
        }
        let upstream = &model_id[prefix.len()..];
        if upstream.is_empty() {
            break;
        }
        let account_id = account
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let provider = Map::from_iter([
            ("id".to_owned(), Value::String(account_id.to_owned())),
            (
                "name".to_owned(),
                Value::String(
                    account
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(account_id)
                        .to_owned(),
                ),
            ),
            (
                "base_url".to_owned(),
                Value::String(codex_base_url(config).to_owned()),
            ),
            ("protocol".to_owned(), Value::String("responses".to_owned())),
            ("auth_mode".to_owned(), Value::String("account".to_owned())),
            ("account".to_owned(), Value::Object(account.clone())),
        ]);
        let model = subscription_model(config, upstream, Some(account))
            .map(|model| native_route_model(model, model_id, upstream))
            .unwrap_or_else(|| route_model(model_id, upstream));
        return resolved_route_from_parts(
            model_id,
            provider,
            model,
            RouteSource::SubscriptionAccount,
        );
    }

    let forward = providers
        .iter()
        .filter_map(Value::as_object)
        .filter(|provider| {
            enabled(provider)
                && provider.get("auth_mode").and_then(Value::as_str) == Some("forward")
        })
        .collect::<Vec<_>>();
    if let [provider] = forward.as_slice() {
        let provider_base = provider
            .get("base_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_end_matches('/');
        let native_base = codex_base_url(config).trim_end_matches('/');
        let model = if !provider_base.is_empty() && provider_base == native_base {
            subscription_model(config, model_id, None)
                .map(|model| native_route_model(model, model_id, model_id))
        } else {
            None
        }
        .unwrap_or_else(|| route_model(model_id, model_id));
        return resolved_route_from_parts(
            model_id,
            (*provider).clone(),
            model,
            RouteSource::ForwardProvider,
        );
    }

    if forward.is_empty()
        && !model_id.contains('/')
        && let Some(model) = subscription_model(config, model_id, None)
    {
        let mut provider = Map::from_iter([
            ("id".to_owned(), Value::String("codex-native".to_owned())),
            ("name".to_owned(), Value::String("Native Codex".to_owned())),
            (
                "base_url".to_owned(),
                Value::String(codex_base_url(config).to_owned()),
            ),
            ("protocol".to_owned(), Value::String("responses".to_owned())),
            ("auth_mode".to_owned(), Value::String("forward".to_owned())),
            ("implicit_native".to_owned(), Value::Bool(true)),
        ]);
        if let Some(path) = config
            .get("_native_auth_path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        {
            provider.insert(
                "_native_auth_path".to_owned(),
                Value::String(path.to_owned()),
            );
        }
        return resolved_route_from_parts(
            model_id,
            provider,
            native_route_model(model, model_id, model_id),
            RouteSource::ImplicitNative,
        );
    }

    Err(RouteResolutionError::UnknownModel(model_id.to_owned()))
}

pub fn resolve_route_without_catalog(
    config: &Value,
    model_id: &str,
) -> Result<ResolvedRoute, RouteResolutionError> {
    resolve_route(config, model_id, |_, _, _| None)
}

fn enabled(value: &Map<String, Value>) -> bool {
    value.get("enabled").and_then(Value::as_bool) != Some(false)
}

fn codex_base_url(config: &Map<String, Value>) -> &str {
    config
        .get("codex_base_url")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_CODEX_BASE_URL)
}

fn route_model(requested: &str, upstream: &str) -> Map<String, Value> {
    Map::from_iter([
        ("id".to_owned(), Value::String(requested.to_owned())),
        ("upstream_id".to_owned(), Value::String(upstream.to_owned())),
    ])
}

fn native_route_model(
    mut model: Map<String, Value>,
    requested: &str,
    upstream: &str,
) -> Map<String, Value> {
    model.insert("id".to_owned(), Value::String(requested.to_owned()));
    model.insert("upstream_id".to_owned(), Value::String(upstream.to_owned()));
    let context_window = model
        .get("context_window")
        .and_then(|value| match value {
            Value::Number(number) => number.as_i64(),
            Value::String(value) => value.parse::<i64>().ok(),
            Value::Bool(value) => Some(i64::from(*value)),
            _ => None,
        })
        .unwrap_or(0);
    if context_window > 0 {
        let mut sources = model
            .get("capability_sources")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if !sources.get("context_window").is_some_and(Value::is_object) {
            sources.insert(
                "context_window".to_owned(),
                serde_json::json!({
                    "source": "official",
                    "confidence": 0.95,
                    "observed_at": null
                }),
            );
        }
        model.insert("capability_sources".to_owned(), Value::Object(sources));
    }
    model
}
