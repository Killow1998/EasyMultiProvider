use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The policy layer used by the future socket client. It deliberately owns no
/// sockets and no request bytes: protocol projection keeps request content,
/// while this layer decides transport identity, limits, and reusable pools.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(300);
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const DEFAULT_NON_STREAM_WALL_CLOCK: Duration = Duration::from_secs(300);
pub const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_millis(100);
pub const DEFAULT_MAX_IDLE_PER_ROUTE: usize = 32;
pub const DEFAULT_MAX_IDLE_TOTAL: usize = 128;
pub const DEFAULT_IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

const MAX_HEADER_NAME_BYTES: usize = 256;
const MAX_HEADER_VALUE_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
}

impl HttpMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpClientPolicyError {
    InvalidMethod,
    InvalidUrl,
    InvalidProxy,
    InvalidHeaderName,
    InvalidHeaderValue,
    HeaderTooLarge,
    InvalidTimeout,
    InvalidPoolKey,
}

impl fmt::Display for HttpClientPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidMethod => "invalid HTTP method",
            Self::InvalidUrl => "invalid upstream URL",
            Self::InvalidProxy => "invalid configured proxy",
            Self::InvalidHeaderName => "invalid HTTP header name",
            Self::InvalidHeaderValue => "invalid HTTP header value",
            Self::HeaderTooLarge => "HTTP header exceeds EMP's size limit",
            Self::InvalidTimeout => "HTTP timeout must be greater than zero",
            Self::InvalidPoolKey => "invalid HTTP connection pool key",
        })
    }
}

impl std::error::Error for HttpClientPolicyError {}

type HttpResult<T> = Result<T, HttpClientPolicyError>;

#[derive(Clone, PartialEq, Eq)]
pub enum ProxyOrigin {
    Direct,
    Proxy { origin: String, token: String },
}

impl fmt::Debug for ProxyOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct => formatter.write_str("Direct"),
            Self::Proxy { .. } => formatter.write_str("Proxy { configured: true }"),
        }
    }
}

impl ProxyOrigin {
    fn proxy(raw: &str) -> HttpResult<Self> {
        let target = parse_url(raw).map_err(|_| HttpClientPolicyError::InvalidProxy)?;
        if target.scheme != "http"
            && target.scheme != "https"
            && target.scheme != "socks5"
            && target.scheme != "socks5h"
        {
            return Err(HttpClientPolicyError::InvalidProxy);
        }
        let host = target.host.ok_or(HttpClientPolicyError::InvalidProxy)?;
        let port = target.port.unwrap_or(match target.scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => 1080,
        });
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let origin = format!("{}://{}:{}", target.scheme, host, port);
        // Pool identity includes proxy credentials exactly as Python's manager
        // key does, but only the digest survives planning or diagnostics.
        let token = safe_token(raw.to_owned());
        Ok(Self::Proxy { origin, token })
    }

    pub const fn is_proxy(&self) -> bool {
        matches!(self, Self::Proxy { .. })
    }

    pub fn pool_token(&self) -> &str {
        match self {
            Self::Direct => "d15690f08a575024650b01ffac892cfd2b93e6c57c140f1b6d9e47753cabd579",
            Self::Proxy { token, .. } => token,
        }
    }
}

/// URL credentials are never represented outside the temporary parse result.
#[derive(Clone, PartialEq, Eq)]
pub struct RouteIdentity {
    pub scheme: String,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
    pub query: Option<String>,
    pub proxy_origin: ProxyOrigin,
    transport_proxy_url: Option<String>,
}

impl fmt::Debug for RouteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RouteIdentity")
            .field("scheme", &self.scheme)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("path", &self.path)
            .field("has_query", &self.query.is_some())
            .field("proxy", &self.proxy_origin)
            .finish()
    }
}

impl RouteIdentity {
    pub(crate) fn transport_proxy_url(&self) -> Option<String> {
        self.transport_proxy_url.clone()
    }

    pub fn route_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let authority = self
            .port
            .map_or_else(|| host.clone(), |port| format!("{host}:{port}"));
        let mut url = format!("{}://{authority}{}", self.scheme, self.path);
        if let Some(query) = self.query.as_deref() {
            url.push('?');
            url.push_str(query);
        }
        url
    }

    pub fn connection_pool_key(&self) -> String {
        let port = self.port.unwrap_or(match self.scheme.as_str() {
            "https" => 443,
            _ => 80,
        });
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!(
            "{}://{}:{}|{}",
            self.scheme,
            host,
            port,
            self.proxy_origin.pool_token()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlTarget {
    scheme: String,
    userinfo: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    path: String,
    query: Option<String>,
}

fn parse_url(raw: &str) -> HttpResult<UrlTarget> {
    let (scheme, remainder) = raw
        .split_once(':')
        .ok_or(HttpClientPolicyError::InvalidUrl)?;
    if scheme.is_empty()
        || !scheme
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphabetic())
        || !scheme.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
        })
    {
        return Err(HttpClientPolicyError::InvalidUrl);
    }
    let Some(authority_and_rest) = remainder.strip_prefix("//") else {
        return Err(HttpClientPolicyError::InvalidUrl);
    };
    let authority_end = authority_and_rest
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_rest.len());
    let authority = &authority_and_rest[..authority_end];
    let rest = &authority_and_rest[authority_end..];
    let (before_fragment, _) = rest
        .split_once('#')
        .map_or((rest, None), |(head, tail)| (head, Some(tail)));
    let (path_and_query, query) = before_fragment
        .split_once('?')
        .map_or((before_fragment, None), |(head, tail)| (head, Some(tail)));
    let (userinfo, host_port) = authority.rsplit_once('@').map_or_else(
        || (None, authority),
        |(userinfo, host_port)| (Some(userinfo.to_owned()), host_port),
    );
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return Err(HttpClientPolicyError::InvalidUrl);
        };
        let port = suffix
            .strip_prefix(':')
            .map(str::parse::<u16>)
            .transpose()
            .map_err(|_| HttpClientPolicyError::InvalidUrl)?;
        (host.to_ascii_lowercase(), port)
    } else {
        let (host, port) = host_port
            .rsplit_once(':')
            .map_or((host_port, None), |(host, port)| {
                (host, port.parse::<u16>().ok())
            });
        let port = match (host_port.rsplit_once(':'), port) {
            (Some(_), None) => return Err(HttpClientPolicyError::InvalidUrl),
            _ => port,
        };
        (host.to_ascii_lowercase(), port)
    };
    Ok(UrlTarget {
        scheme: scheme.to_ascii_lowercase(),
        userinfo,
        host: (!host.is_empty()).then_some(host),
        port,
        path: if path_and_query.is_empty() {
            "/".to_owned()
        } else {
            path_and_query.to_owned()
        },
        query: query.map(str::to_owned),
    })
}

#[derive(Clone, PartialEq, Eq)]
pub struct ProxyEnvironment {
    pub http: Option<String>,
    pub https: Option<String>,
    pub ws: Option<String>,
    pub wss: Option<String>,
    pub socks: Option<String>,
    pub all: Option<String>,
    pub no_proxy: Vec<String>,
}

impl fmt::Debug for ProxyEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyEnvironment")
            .field("http_configured", &self.http.is_some())
            .field("https_configured", &self.https.is_some())
            .field("ws_configured", &self.ws.is_some())
            .field("wss_configured", &self.wss.is_some())
            .field("socks_configured", &self.socks.is_some())
            .field("all_configured", &self.all.is_some())
            .field("no_proxy_entries", &self.no_proxy.len())
            .finish()
    }
}

impl Default for ProxyEnvironment {
    fn default() -> Self {
        Self {
            http: None,
            https: None,
            ws: None,
            wss: None,
            socks: None,
            all: None,
            no_proxy: loopback_no_proxy(),
        }
    }
}

impl ProxyEnvironment {
    /// Snapshot conventional proxy environment variables. Secrets remain
    /// inside the transport policy and Debug only reports configured lanes.
    pub fn capture() -> Self {
        fn value(names: &[&str]) -> Option<String> {
            names
                .iter()
                .find_map(std::env::var_os)
                .and_then(|value| value.into_string().ok())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        }

        let mut no_proxy = ["no_proxy", "NO_PROXY"]
            .into_iter()
            .filter_map(|name| value(&[name]))
            .flat_map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for loopback in loopback_no_proxy() {
            if !no_proxy.iter().any(|value| value == &loopback) {
                no_proxy.push(loopback);
            }
        }
        Self {
            http: value(&["http_proxy", "HTTP_PROXY"]),
            https: value(&["https_proxy", "HTTPS_PROXY"]),
            ws: value(&["ws_proxy", "WS_PROXY"]),
            wss: value(&["wss_proxy", "WSS_PROXY"]),
            socks: value(&["socks_proxy", "SOCKS_PROXY"]),
            all: value(&["all_proxy", "ALL_PROXY"]),
            no_proxy,
        }
    }

    pub(crate) fn has_proxy(&self) -> bool {
        self.http.is_some()
            || self.https.is_some()
            || self.ws.is_some()
            || self.wss.is_some()
            || self.socks.is_some()
            || self.all.is_some()
    }

    /// Resolve environment-first proxy settings, consulting the operating
    /// system only when no explicit proxy variable is configured.
    pub fn capture_current() -> ProxySnapshot {
        let environment = Self::capture();
        if environment.has_proxy() {
            return ProxySnapshot::select(environment, None);
        }
        ProxySnapshot::select(environment, crate::system_proxy::capture())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxySource {
    Environment,
    System,
    Direct,
}

impl ProxySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Environment => "environment",
            Self::System => "system",
            Self::Direct => "direct",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxySnapshot {
    pub environment: ProxyEnvironment,
    pub source: ProxySource,
}

impl ProxySnapshot {
    pub(crate) fn select(environment: ProxyEnvironment, system: Option<ProxyEnvironment>) -> Self {
        if environment.has_proxy() {
            return Self {
                environment,
                source: ProxySource::Environment,
            };
        }
        let Some(mut system) = system.filter(ProxyEnvironment::has_valid_proxy) else {
            return Self {
                environment,
                source: ProxySource::Direct,
            };
        };
        for bypass in environment.no_proxy {
            if !system.no_proxy.contains(&bypass) {
                system.no_proxy.push(bypass);
            }
        }
        Self {
            environment: system,
            source: ProxySource::System,
        }
    }
}

impl ProxyEnvironment {
    fn has_valid_proxy(&self) -> bool {
        [
            self.http.as_deref(),
            self.https.as_deref(),
            self.ws.as_deref(),
            self.wss.as_deref(),
            self.socks.as_deref(),
            self.all.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|proxy| ProxyOrigin::proxy(proxy).is_ok())
    }
}

fn loopback_no_proxy() -> Vec<String> {
    ["localhost", "127.0.0.1", "::1"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

pub fn is_loopback(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn host_matches_bypass(host: &str, pattern: &str) -> bool {
    let pattern = pattern.trim().trim_start_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    !pattern.is_empty() && (host == pattern || host.ends_with(&format!(".{pattern}")))
}

fn no_proxy_matches(host: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        pattern == "*"
            || (pattern.eq_ignore_ascii_case("<local>") && !host.contains('.'))
            || host_matches_bypass(host, pattern)
            || host_matches_wildcard(host, pattern)
    })
}

fn host_matches_wildcard(host: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return false;
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let mut remaining = host.as_str();
    let pattern = pattern.trim().to_ascii_lowercase();
    let mut parts = pattern.split('*').peekable();
    let mut first = true;
    while let Some(part) = parts.next() {
        if part.is_empty() {
            first = false;
            continue;
        }
        let Some(position) = remaining.find(part) else {
            return false;
        };
        if first && position != 0 {
            return false;
        }
        if parts.peek().is_none() && position + part.len() != remaining.len() {
            return false;
        }
        remaining = &remaining[position + part.len()..];
        first = false;
    }
    true
}

type ProxyEnvironmentResolver = dyn Fn() -> ProxyEnvironment + Send + Sync;

#[derive(Clone)]
pub struct ProxyPolicy {
    explicit: Option<String>,
    environment: Option<ProxyEnvironment>,
    resolver: Option<Arc<ProxyEnvironmentResolver>>,
    bypass_loopback: bool,
}

impl PartialEq for ProxyPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.explicit == other.explicit
            && self.environment == other.environment
            && self.bypass_loopback == other.bypass_loopback
            && match (&self.resolver, &other.resolver) {
                (None, None) => true,
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                _ => false,
            }
    }
}

impl Eq for ProxyPolicy {}

impl fmt::Debug for ProxyPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProxyPolicy")
            .field("explicit_configured", &self.explicit.is_some())
            .field("environment_configured", &self.environment.is_some())
            .field("dynamic_resolver_configured", &self.resolver.is_some())
            .field("bypass_loopback", &self.bypass_loopback)
            .finish()
    }
}

impl Default for ProxyPolicy {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl ProxyPolicy {
    pub fn new(explicit: Option<String>, environment: Option<ProxyEnvironment>) -> Self {
        Self {
            explicit,
            environment,
            resolver: None,
            bypass_loopback: true,
        }
    }

    pub fn explicit(proxy: Option<String>) -> Self {
        Self::new(proxy, None)
    }

    pub fn from_environment(environment: ProxyEnvironment) -> Self {
        Self::new(None, Some(environment))
    }

    /// Resolve conventional environment proxy settings for each new request.
    pub fn dynamic_environment() -> Self {
        Self::dynamic_with_resolver(|| ProxyEnvironment::capture_current().environment)
    }

    /// Build a policy with an injectable resolver. Existing request plans keep
    /// the resolved proxy URL they were created with; later plans call this
    /// resolver again.
    pub fn dynamic_with_resolver<F>(resolver: F) -> Self
    where
        F: Fn() -> ProxyEnvironment + Send + Sync + 'static,
    {
        Self {
            explicit: None,
            environment: None,
            resolver: Some(Arc::new(resolver)),
            bypass_loopback: true,
        }
    }

    fn proxy_url_for_target(&self, route: &UrlTarget) -> HttpResult<Option<String>> {
        let host = route.host.as_deref().unwrap_or("");
        if self.bypass_loopback && is_loopback(host) {
            return Ok(None);
        }
        let environment = self
            .resolver
            .as_ref()
            .map(|resolver| resolver())
            .or_else(|| self.environment.clone());
        let bypass = environment
            .as_ref()
            .is_some_and(|settings| no_proxy_matches(host, &settings.no_proxy));
        if bypass {
            return Ok(None);
        }
        if let Some(proxy) = self.explicit.as_deref().filter(|value| !value.is_empty()) {
            ProxyOrigin::proxy(proxy)?;
            return Ok(Some(proxy.to_owned()));
        }
        let Some(settings) = environment.as_ref() else {
            return Ok(None);
        };
        let candidates: &[(&Option<String>, bool)] = match route.scheme.as_str() {
            "https" | "wss" => &[
                (&settings.wss, false),
                (&settings.socks, true),
                (&settings.https, false),
                (&settings.all, false),
            ],
            "http" | "ws" => &[
                (&settings.ws, false),
                (&settings.socks, true),
                (&settings.https, false),
                (&settings.http, false),
                (&settings.all, false),
            ],
            _ => &[(&settings.all, false)],
        };
        let selected = candidates
            .iter()
            .find(|(value, _)| value.as_deref().is_some_and(|value| !value.is_empty()))
            .map(|(value, socks)| {
                let value = value.as_deref().unwrap_or_default();
                if *socks && value.starts_with("http://") {
                    format!("socks5h://{}", &value[7..])
                } else {
                    value.to_owned()
                }
            });
        if let Some(proxy) = selected.as_deref() {
            ProxyOrigin::proxy(proxy)?;
        }
        Ok(selected)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutPolicy {
    pub connect: Duration,
    pub first_event: Duration,
    pub stream_idle: Duration,
    pub non_stream_wall_clock: Duration,
    pub cleanup: Duration,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            connect: DEFAULT_CONNECT_TIMEOUT,
            first_event: DEFAULT_FIRST_EVENT_TIMEOUT,
            stream_idle: DEFAULT_STREAM_IDLE_TIMEOUT,
            non_stream_wall_clock: DEFAULT_NON_STREAM_WALL_CLOCK,
            cleanup: DEFAULT_CLEANUP_TIMEOUT,
        }
    }
}

impl TimeoutPolicy {
    pub fn validate(&self) -> HttpResult<()> {
        for value in [
            self.connect,
            self.first_event,
            self.stream_idle,
            self.non_stream_wall_clock,
            self.cleanup,
        ] {
            if value.is_zero() {
                return Err(HttpClientPolicyError::InvalidTimeout);
            }
        }
        Ok(())
    }

    pub fn non_stream_deadline(&self, started_at: Instant) -> Instant {
        started_at + self.non_stream_wall_clock
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTimeoutError {
    LocalDeadline,
    FirstEventTimeout,
    IdleTimeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingReadState {
    policy: TimeoutPolicy,
    started_at: Instant,
    output_emitted: bool,
}

impl StreamingReadState {
    pub fn new(policy: TimeoutPolicy, started_at: Instant) -> HttpResult<Self> {
        policy.validate()?;
        Ok(Self {
            policy,
            started_at,
            output_emitted: false,
        })
    }

    pub fn observe_output(&mut self) {
        self.output_emitted = true;
    }

    pub fn output_emitted(&self) -> bool {
        self.output_emitted
    }

    pub fn read_timeout_at(&self, now: Instant) -> Result<Duration, StreamTimeoutError> {
        if now < self.started_at {
            return Err(StreamTimeoutError::LocalDeadline);
        }
        let limit = if self.output_emitted {
            self.policy.stream_idle
        } else {
            self.policy.first_event
        };
        let _ = now.checked_duration_since(self.started_at);
        Ok(limit)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct RequestPlan {
    pub method: HttpMethod,
    pub route: RouteIdentity,
    pub headers: BTreeMap<String, String>,
    pub stream: bool,
    pub redirect_policy: RedirectPolicy,
    pub retry_policy: RetryPolicy,
    pub timeout_policy: TimeoutPolicy,
}

impl fmt::Debug for RequestPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RequestPlan")
            .field("method", &self.method)
            .field("route", &self.route)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("stream", &self.stream)
            .field("redirect_policy", &self.redirect_policy)
            .field("retry_policy", &self.retry_policy)
            .field("timeout_policy", &self.timeout_policy)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectPolicy {
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpClientPolicy {
    proxy_policy: ProxyPolicy,
    timeout_policy: TimeoutPolicy,
}

impl Default for HttpClientPolicy {
    fn default() -> Self {
        Self::new(ProxyPolicy::default(), TimeoutPolicy::default())
    }
}

impl HttpClientPolicy {
    pub fn new(proxy_policy: ProxyPolicy, timeout_policy: TimeoutPolicy) -> Self {
        Self {
            proxy_policy,
            timeout_policy,
        }
    }

    pub fn proxy_policy(&self) -> &ProxyPolicy {
        &self.proxy_policy
    }

    pub fn timeout_policy(&self) -> &TimeoutPolicy {
        &self.timeout_policy
    }

    pub fn plan(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        stream: bool,
    ) -> HttpResult<RequestPlan> {
        self.timeout_policy.validate()?;
        let target = parse_url(url)?;
        if !matches!(target.scheme.as_str(), "http" | "https") {
            return Err(HttpClientPolicyError::InvalidUrl);
        }
        if target
            .userinfo
            .as_ref()
            .is_some_and(|value| !value.is_empty())
        {
            return Err(HttpClientPolicyError::InvalidUrl);
        }
        let host = target
            .host
            .clone()
            .ok_or(HttpClientPolicyError::InvalidUrl)?;
        let transport_proxy_url = self.proxy_policy.proxy_url_for_target(&target)?;
        let proxy_origin = transport_proxy_url
            .as_deref()
            .map(ProxyOrigin::proxy)
            .unwrap_or(Ok(ProxyOrigin::Direct))?;
        let mut normalized_headers = BTreeMap::new();
        for (name, value) in headers {
            validate_header_name(&name)?;
            validate_header_value(&value)?;
            normalized_headers.insert(name.to_ascii_lowercase(), value);
        }
        normalized_headers
            .entry("accept-encoding".to_owned())
            .or_insert_with(|| "identity".to_owned());
        Ok(RequestPlan {
            method,
            route: RouteIdentity {
                scheme: target.scheme,
                host,
                port: target.port,
                path: target.path,
                query: target.query,
                proxy_origin,
                transport_proxy_url,
            },
            headers: normalized_headers,
            stream,
            redirect_policy: RedirectPolicy::Disabled,
            retry_policy: RetryPolicy::Disabled,
            timeout_policy: self.timeout_policy,
        })
    }

    pub(crate) fn transport_proxy_for(&self, route: &RouteIdentity) -> HttpResult<Option<String>> {
        Ok(route.transport_proxy_url())
    }
}

fn validate_header_name(name: &str) -> HttpResult<()> {
    if name.is_empty()
        || name.len() > MAX_HEADER_NAME_BYTES
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(
                    character,
                    '!' | '#'
                        | '$'
                        | '%'
                        | '&'
                        | '\''
                        | '*'
                        | '+'
                        | '-'
                        | '.'
                        | '^'
                        | '_'
                        | '`'
                        | '|'
                )
        })
    {
        return Err(HttpClientPolicyError::InvalidHeaderName);
    }
    Ok(())
}

fn validate_header_value(value: &str) -> HttpResult<()> {
    if value.len() > MAX_HEADER_VALUE_BYTES
        || value
            .chars()
            .any(|character| character == '\r' || character == '\n' || character.is_control())
    {
        return Err(HttpClientPolicyError::InvalidHeaderValue);
    }
    Ok(())
}

fn safe_token(value: String) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionPoolPolicy {
    pub max_idle_per_route: usize,
    pub max_idle_total: usize,
    pub idle_timeout: Duration,
}

impl Default for ConnectionPoolPolicy {
    fn default() -> Self {
        Self {
            max_idle_per_route: DEFAULT_MAX_IDLE_PER_ROUTE,
            max_idle_total: DEFAULT_MAX_IDLE_TOTAL,
            idle_timeout: DEFAULT_IDLE_CONNECTION_TIMEOUT,
        }
    }
}

impl ConnectionPoolPolicy {
    pub fn validate(&self) -> HttpResult<()> {
        if self.max_idle_per_route == 0
            || self.max_idle_total < self.max_idle_per_route
            || self.idle_timeout.is_zero()
        {
            return Err(HttpClientPolicyError::InvalidPoolKey);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdleConnection {
    checked_in_at: Instant,
}

#[derive(Debug)]
pub struct IdleConnectionPool {
    policy: ConnectionPoolPolicy,
    routes: HashMap<String, VecDeque<IdleConnection>>,
    total: usize,
}

impl Default for IdleConnectionPool {
    fn default() -> Self {
        Self::new(ConnectionPoolPolicy::default())
    }
}

impl IdleConnectionPool {
    pub fn new(policy: ConnectionPoolPolicy) -> Self {
        Self {
            policy,
            routes: HashMap::new(),
            total: 0,
        }
    }

    pub fn policy(&self) -> &ConnectionPoolPolicy {
        &self.policy
    }

    pub fn total_idle(&self) -> usize {
        self.total
    }

    pub fn route_idle(&self, key: &str) -> usize {
        self.routes.get(key).map_or(0, VecDeque::len)
    }

    pub fn check_in(&mut self, key: &str, now: Instant) -> HttpResult<()> {
        self.policy.validate()?;
        if key.is_empty() {
            return Err(HttpClientPolicyError::InvalidPoolKey);
        }
        let route = self.routes.entry(key.to_owned()).or_default();
        route.push_back(IdleConnection { checked_in_at: now });
        self.total += 1;
        while route.len() > self.policy.max_idle_per_route {
            route.pop_front();
            self.total -= 1;
        }
        while self.total > self.policy.max_idle_total {
            let Some((oldest_key, _)) = self
                .routes
                .iter()
                .filter_map(|(key, route)| route.front().map(|value| (key, value.checked_in_at)))
                .min_by_key(|(_, checked_in_at)| *checked_in_at)
            else {
                break;
            };
            let oldest_key = oldest_key.clone();
            if let Some(route) = self.routes.get_mut(&oldest_key) {
                if route.pop_front().is_some() {
                    self.total -= 1;
                }
                if route.is_empty() {
                    self.routes.remove(&oldest_key);
                }
            }
        }
        Ok(())
    }

    pub fn check_out(&mut self, key: &str, now: Instant) -> Option<Instant> {
        let mut result = None;
        let empty = {
            let route = self.routes.get_mut(key)?;
            while let Some(connection) = route.front() {
                let expired = now
                    .checked_duration_since(connection.checked_in_at)
                    .is_none_or(|age| age >= self.policy.idle_timeout);
                if expired {
                    route.pop_front();
                    self.total -= 1;
                    continue;
                }
                result = Some(connection.checked_in_at);
                route.pop_front();
                self.total -= 1;
                break;
            }
            route.is_empty()
        };
        if empty {
            self.routes.remove(key);
        }
        result
    }
}
