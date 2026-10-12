use crate::http_policy::{
    ConnectionPoolPolicy, HttpClientPolicy, HttpClientPolicyError, HttpMethod, RequestPlan,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use reqwest::{Certificate, Client, Method, Proxy};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::Instant;
use url::Url;

const DEFAULT_MAX_PROXY_POOLS: usize = 4;
mod response;
pub use response::HttpResponse;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RedirectMode {
    Reject,
    PreserveStatus,
    Follow,
}

#[derive(Clone)]
struct RedirectOrigin {
    scheme: String,
    host: String,
    port: u16,
}

impl RedirectOrigin {
    fn from_plan(plan: &RequestPlan) -> Self {
        let route = &plan.route;
        let port = route.port.unwrap_or(match route.scheme.as_str() {
            "https" => 443,
            _ => 80,
        });
        Self {
            scheme: route.scheme.clone(),
            host: route.host.clone(),
            port,
        }
    }

    fn matches(&self, url: &Url) -> bool {
        // `Url::host_str` brackets IPv6 literals; the route host may not.
        let unbracketed = |host: &str| {
            host.strip_prefix('[')
                .and_then(|host| host.strip_suffix(']'))
                .unwrap_or(host)
                .to_owned()
        };
        self.scheme == url.scheme()
            && url.host_str().is_some_and(|host| {
                unbracketed(host).eq_ignore_ascii_case(&unbracketed(&self.host))
            })
            && url.port_or_known_default() == Some(self.port)
    }

    fn cache_key(&self) -> String {
        format!("{}://{}:{}", self.scheme, self.host, self.port)
    }

    fn policy(self) -> Policy {
        Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 || !self.matches(attempt.url()) {
                // Returning the redirect response leaves the caller in control
                // and ensures request credentials never reach another origin.
                attempt.stop()
            } else {
                attempt.follow()
            }
        })
    }
}

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
        self.open_with_redirects(method, url, headers, body, stream, RedirectMode::Reject)
            .await
    }

    /// Open a request without following redirects and return redirect responses
    /// as ordinary upstream statuses. This is for protocols that must preserve
    /// a 3xx handshake rather than treating it as a transport failure.
    pub async fn open_status(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Option<Vec<u8>>,
        stream: bool,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.open_with_redirects(
            method,
            url,
            headers,
            body,
            stream,
            RedirectMode::PreserveStatus,
        )
        .await
    }

    /// Follow bounded redirects only while they remain on the initial origin.
    ///
    /// Scheme, host, and effective port must all match the initial URL. This
    /// prevents caller credentials, including provider-specific API-key
    /// headers, from being forwarded to another origin or across a downgrade.
    /// Redirects remain disabled for the default [`Self::open`] path.
    pub async fn open_following_redirects(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Option<Vec<u8>>,
        stream: bool,
    ) -> Result<HttpResponse, HttpTransportError> {
        self.open_with_redirects(method, url, headers, body, stream, RedirectMode::Follow)
            .await
    }

    async fn open_with_redirects(
        &self,
        method: HttpMethod,
        url: &str,
        headers: BTreeMap<String, String>,
        body: Option<Vec<u8>>,
        stream: bool,
        redirect_mode: RedirectMode,
    ) -> Result<HttpResponse, HttpTransportError> {
        let plan = self.policy.plan(method, url, headers, stream)?;
        let client = self.client_for(&plan, redirect_mode)?;
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
        if redirect_mode == RedirectMode::Reject && response.status().is_redirection() {
            drop(response);
            return Err(HttpTransportError::new(
                HttpTransportErrorKind::RedirectDisabled,
            ));
        }
        Ok(HttpResponse::new(response, plan, started_at))
    }

    #[cfg_attr(feature = "hotpath", hotpath::measure)]
    fn client_for(
        &self,
        plan: &RequestPlan,
        redirect_mode: RedirectMode,
    ) -> Result<Client, HttpTransportError> {
        let redirect_identity = match redirect_mode {
            RedirectMode::Reject => "reject".to_owned(),
            RedirectMode::PreserveStatus => "preserve".to_owned(),
            RedirectMode::Follow => {
                format!(
                    "same-origin:{}",
                    RedirectOrigin::from_plan(plan).cache_key()
                )
            }
        };
        let identity = format!(
            "{}|redirect={redirect_identity}",
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
        let redirect_policy = match redirect_mode {
            RedirectMode::Reject | RedirectMode::PreserveStatus => Policy::none(),
            RedirectMode::Follow => RedirectOrigin::from_plan(plan).policy(),
        };
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(redirect_policy)
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
        while clients.len() > self.config.max_proxy_pools.min(DEFAULT_MAX_PROXY_POOLS) {
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

#[cfg(test)]
mod tests;
