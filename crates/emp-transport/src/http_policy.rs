use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
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

mod pool;
mod proxy;
pub use pool::ConnectionPoolPolicy;
pub use proxy::{ProxyEnvironment, ProxyPolicy, ProxySnapshot, ProxySource};
