use crate::http_policy::{
    ConnectionPoolPolicy, HttpClientPolicy, HttpClientPolicyError, HttpMethod, RequestPlan,
    TimeoutPolicy,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Certificate, Client, Method, Proxy, StatusCode};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;

const DEFAULT_MAX_PROXY_POOLS: usize = 4;
const CLEANUP_BYTE_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpTransportErrorKind {
    InvalidRequest,
    ClientBuild,
    ConnectTimeout,
    ReadTimeout,
    ResponseTooLarge,
    Network,
    RedirectDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpTransportError {
    kind: HttpTransportErrorKind,
}

impl HttpTransportError {
    const fn new(kind: HttpTransportErrorKind) -> Self {
        Self { kind }
    }

    pub const fn kind(self) -> HttpTransportErrorKind {
        self.kind
    }
}

impl fmt::Display for HttpTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self.kind {
            HttpTransportErrorKind::InvalidRequest => "invalid upstream HTTP request",
            HttpTransportErrorKind::ClientBuild => "failed to build upstream HTTP client",
            HttpTransportErrorKind::ConnectTimeout => "upstream connection timed out",
            HttpTransportErrorKind::ReadTimeout => "upstream read timed out",
            HttpTransportErrorKind::ResponseTooLarge => "upstream response is too large",
            HttpTransportErrorKind::Network => "upstream network request failed",
            HttpTransportErrorKind::RedirectDisabled => "upstream redirects are disabled",
        })
    }
}

impl std::error::Error for HttpTransportError {}

impl From<HttpClientPolicyError> for HttpTransportError {
    fn from(_: HttpClientPolicyError) -> Self {
        Self::new(HttpTransportErrorKind::InvalidRequest)
    }
}

#[derive(Clone)]
pub struct HttpClientConfig {
    pub pool: ConnectionPoolPolicy,
    pub max_proxy_pools: usize,
    root_certificates: Vec<Certificate>,
    only_configured_root_certificates: bool,
    dns_overrides: Vec<(String, SocketAddr)>,
}

impl fmt::Debug for HttpClientConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClientConfig")
            .field("pool", &self.pool)
            .field("max_proxy_pools", &self.max_proxy_pools)
            .field(
                "additional_root_certificates",
                &self.root_certificates.len(),
            )
            .field("dns_overrides", &self.dns_overrides.len())
            .finish()
    }
}

impl Default for HttpClientConfig {
    fn default() -> Self {
        Self {
            pool: ConnectionPoolPolicy::default(),
            max_proxy_pools: DEFAULT_MAX_PROXY_POOLS,
            root_certificates: Vec::new(),
            only_configured_root_certificates: false,
            dns_overrides: Vec::new(),
        }
    }
}

impl HttpClientConfig {
    pub fn add_root_certificate_der(
        &mut self,
        certificate: &[u8],
    ) -> Result<(), HttpTransportError> {
        let certificate = Certificate::from_der(certificate)
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ClientBuild))?;
        self.root_certificates.push(certificate);
        Ok(())
    }

    /// Trust only roots supplied through [`Self::add_root_certificate_der`].
    ///
    /// The default merges configured roots with the platform trust store. This
    /// opt-in mode is useful for hermetic deployments and deterministic TLS
    /// tests where the platform verifier must not participate.
    pub fn use_only_configured_root_certificates(&mut self) -> Result<(), HttpTransportError> {
        if self.root_certificates.is_empty() {
            return Err(HttpTransportError::new(HttpTransportErrorKind::ClientBuild));
        }
        self.only_configured_root_certificates = true;
        Ok(())
    }

    pub fn add_dns_override(
        &mut self,
        host: impl Into<String>,
        address: SocketAddr,
    ) -> Result<(), HttpTransportError> {
        let host = host.into();
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(HttpTransportError::new(HttpTransportErrorKind::ClientBuild));
        }
        self.dns_overrides.push((host, address));
        Ok(())
    }

    fn validate(&self) -> Result<(), HttpTransportError> {
        self.pool.validate()?;
        if self.max_proxy_pools == 0 {
            return Err(HttpTransportError::new(HttpTransportErrorKind::ClientBuild));
        }
        Ok(())
    }
}

struct CachedClient {
    identity: String,
    client: Client,
}

pub struct HttpClient {
    policy: HttpClientPolicy,
    config: HttpClientConfig,
    clients: Mutex<VecDeque<CachedClient>>,
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("policy", &self.policy)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl HttpClient {
    pub fn new(policy: HttpClientPolicy) -> Result<Self, HttpTransportError> {
        Self::with_config(policy, HttpClientConfig::default())
    }

    pub fn with_config(
        policy: HttpClientPolicy,
        config: HttpClientConfig,
    ) -> Result<Self, HttpTransportError> {
        policy.timeout_policy().validate()?;
        config.validate()?;
        Ok(Self {
            policy,
            config,
            clients: Mutex::new(VecDeque::new()),
        })
    }

    pub fn policy(&self) -> &HttpClientPolicy {
        &self.policy
    }

    /// Resolve the immutable proxy selected for one WebSocket route.
    pub fn websocket_proxy_for(&self, url: &str) -> Result<Option<String>, HttpTransportError> {
        let policy_url = if let Some(rest) = url.strip_prefix("ws://") {
            format!("http://{rest}")
        } else if let Some(rest) = url.strip_prefix("wss://") {
            format!("https://{rest}")
        } else {
            return Err(HttpTransportError::new(
                HttpTransportErrorKind::InvalidRequest,
            ));
        };
        let plan = self
            .policy
            .plan(HttpMethod::Get, &policy_url, BTreeMap::new(), true)?;
        self.policy
            .transport_proxy_for(&plan.route)
            .map_err(Into::into)
    }

    pub async fn open(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Option<Vec<u8>>,
        stream: bool,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.open_with_redirects(method, url, headers, body, stream, false)
            .await
    }

    /// Open a request using the standard bounded HTTP redirect policy.
    ///
    /// Redirects remain disabled for the default [`Self::open`] path. Callers
    /// should opt in only when their source behavior follows redirects.
    pub async fn open_following_redirects(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Option<Vec<u8>>,
        stream: bool,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.open_with_redirects(method, url, headers, body, stream, true)
            .await
    }

    async fn open_with_redirects(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Option<Vec<u8>>,
        stream: bool,
        follow_redirects: bool,
    ) -> Result<HttpResponse, HttpTransportError> {
        let plan = self.policy.plan(method, url, headers, stream)?;
        let client = self.client_for(&plan, follow_redirects)?;
        let started_at = Instant::now();
        let mut request = client.request(to_reqwest_method(method), plan.route.route_url());
        let headers = to_header_map(&plan.headers)?;
        request = request.headers(headers);
        if let Some(body) = body {
            request = request.body(body);
        }
        let send_timeout = if stream {
            plan.timeout_policy.first_event
        } else {
            plan.timeout_policy.non_stream_wall_clock
        };
        let response = tokio::time::timeout(send_timeout, request.send())
            .await
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ConnectTimeout))?
            .map_err(map_send_error)?;
        if !follow_redirects && response.status().is_redirection() {
            drop(response);
            return Err(HttpTransportError::new(
                HttpTransportErrorKind::RedirectDisabled,
            ));
        }
        Ok(HttpResponse::new(response, plan, started_at))
    }

    fn client_for(
        &self,
        plan: &RequestPlan,
        follow_redirects: bool,
    ) -> Result<Client, HttpTransportError> {
        let identity = format!(
            "{}|follow_redirects={follow_redirects}",
            plan.route.proxy_origin.pool_token()
        );
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(position) = clients.iter().position(|entry| entry.identity == identity) {
            let entry = clients.remove(position).expect("cached client exists");
            let client = entry.client.clone();
            clients.push_back(entry);
            return Ok(client);
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(if follow_redirects {
                reqwest::redirect::Policy::limited(10)
            } else {
                reqwest::redirect::Policy::none()
            })
            .retry(reqwest::retry::never())
            .connect_timeout(plan.timeout_policy.connect)
            .pool_idle_timeout(self.config.pool.idle_timeout)
            .pool_max_idle_per_host(self.config.pool.max_idle_per_route);
        if self.config.only_configured_root_certificates {
            builder = builder.tls_certs_only(self.config.root_certificates.clone());
        } else if !self.config.root_certificates.is_empty() {
            builder = builder.tls_certs_merge(self.config.root_certificates.clone());
        }
        for (host, address) in &self.config.dns_overrides {
            builder = builder.resolve(host, *address);
        }
        if let Some(proxy_url) = self.policy.transport_proxy_for(&plan.route)? {
            let proxy = Proxy::all(proxy_url)
                .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ClientBuild))?;
            builder = builder.proxy(proxy);
        }
        let client = builder
            .build()
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ClientBuild))?;
        clients.push_back(CachedClient {
            identity: identity.to_owned(),
            client: client.clone(),
        });
        while clients.len() > self.config.max_proxy_pools {
            clients.pop_front();
        }
        Ok(client)
    }
}

fn to_reqwest_method(method: HttpMethod) -> Method {
    match method {
        HttpMethod::Get => Method::GET,
        HttpMethod::Post => Method::POST,
        HttpMethod::Put => Method::PUT,
        HttpMethod::Patch => Method::PATCH,
        HttpMethod::Delete => Method::DELETE,
        HttpMethod::Head => Method::HEAD,
        HttpMethod::Options => Method::OPTIONS,
    }
}

fn to_header_map(headers: &BTreeMap<String, String>) -> Result<HeaderMap, HttpTransportError> {
    let mut result = HeaderMap::with_capacity(headers.len());
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::InvalidRequest))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::InvalidRequest))?;
        result.insert(name, value);
    }
    Ok(result)
}

fn map_send_error(error: reqwest::Error) -> HttpTransportError {
    if error.is_timeout() {
        HttpTransportError::new(HttpTransportErrorKind::ConnectTimeout)
    } else {
        HttpTransportError::new(HttpTransportErrorKind::Network)
    }
}

fn map_read_error(error: reqwest::Error) -> HttpTransportError {
    if error.is_timeout() {
        HttpTransportError::new(HttpTransportErrorKind::ReadTimeout)
    } else {
        HttpTransportError::new(HttpTransportErrorKind::Network)
    }
}

pub struct HttpResponse {
    response: Option<reqwest::Response>,
    status: StatusCode,
    headers: HeaderMap,
    timeout_policy: TimeoutPolicy,
    non_stream_deadline: Instant,
    stream: bool,
    output_emitted: bool,
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status.as_u16())
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .field("stream", &self.stream)
            .field("output_emitted", &self.output_emitted)
            .finish_non_exhaustive()
    }
}

impl HttpResponse {
    fn new(response: reqwest::Response, plan: RequestPlan, started_at: Instant) -> Self {
        Self {
            status: response.status(),
            headers: response.headers().clone(),
            response: Some(response),
            timeout_policy: plan.timeout_policy,
            non_stream_deadline: plan.timeout_policy.non_stream_deadline(started_at),
            stream: plan.stream,
            output_emitted: false,
        }
    }

    pub fn status(&self) -> u16 {
        self.status.as_u16()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Iterate string-valued response headers without copying them.
    ///
    /// Duplicate names are preserved; non-visible-ASCII values are omitted. This
    /// accessor never logs header values.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.headers.iter().filter_map(|(name, value)| {
            let value = value.to_str().ok()?;
            Some((name.as_str(), value))
        })
    }

    pub fn is_stream(&self) -> bool {
        self.stream
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, HttpTransportError> {
        let timeout = if self.output_emitted {
            self.timeout_policy.stream_idle
        } else {
            self.timeout_policy.first_event
        };
        let response = self
            .response
            .as_mut()
            .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::InvalidRequest))?;
        let chunk = tokio::time::timeout(timeout, response.chunk())
            .await
            .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?
            .map_err(map_read_error)?;
        if chunk.as_ref().is_some_and(|chunk| !chunk.is_empty()) {
            self.output_emitted = true;
        }
        Ok(chunk.map(|chunk| chunk.to_vec()))
    }

    pub async fn read_all(self) -> Result<Vec<u8>, HttpTransportError> {
        self.read_limited(usize::MAX).await
    }

    pub async fn read_limited(self, limit: usize) -> Result<Vec<u8>, HttpTransportError> {
        self.read_body(limit, false).await
    }

    /// Read at most `limit` bytes and discard the rest of this response. Unlike
    /// read_limited, a larger declared or streamed body is not an error.
    pub async fn read_prefix(self, limit: usize) -> Result<Vec<u8>, HttpTransportError> {
        self.read_body(limit, true).await
    }

    async fn read_body(
        mut self,
        limit: usize,
        prefix: bool,
    ) -> Result<Vec<u8>, HttpTransportError> {
        if prefix && limit == 0 {
            return Ok(Vec::new());
        }
        if !prefix
            && self
                .header("content-length")
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > limit)
        {
            return Err(HttpTransportError::new(
                HttpTransportErrorKind::ResponseTooLarge,
            ));
        }
        let remaining = self
            .non_stream_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?;
        let mut response = self
            .response
            .take()
            .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::InvalidRequest))?;
        let deadline = Instant::now() + remaining;
        let mut result = Vec::new();
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?;
            let chunk = tokio::time::timeout(remaining, response.chunk())
                .await
                .map_err(|_| HttpTransportError::new(HttpTransportErrorKind::ReadTimeout))?
                .map_err(map_read_error)?;
            let Some(chunk) = chunk else {
                return Ok(result);
            };
            if prefix {
                let count = chunk.len().min(limit - result.len());
                result.extend_from_slice(&chunk[..count]);
                if result.len() == limit {
                    return Ok(result);
                }
                continue;
            }
            let next_length = result
                .len()
                .checked_add(chunk.len())
                .ok_or_else(|| HttpTransportError::new(HttpTransportErrorKind::ResponseTooLarge))?;
            if next_length > limit {
                return Err(HttpTransportError::new(
                    HttpTransportErrorKind::ResponseTooLarge,
                ));
            }
            result.extend_from_slice(&chunk);
        }
    }

    pub async fn finish(mut self) {
        let Some(mut response) = self.response.take() else {
            return;
        };
        let deadline = Instant::now() + self.timeout_policy.cleanup;
        let mut remaining = CLEANUP_BYTE_LIMIT;
        while remaining > 0 {
            let Some(timeout) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            let chunk = match tokio::time::timeout(timeout, response.chunk()).await {
                Ok(Ok(Some(chunk))) => chunk,
                Ok(Ok(None)) => return,
                Ok(Err(_)) | Err(_) => break,
            };
            if chunk.len() > remaining {
                break;
            }
            remaining -= chunk.len();
        }
    }
}
