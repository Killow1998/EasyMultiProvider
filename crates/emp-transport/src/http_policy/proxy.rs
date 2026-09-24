//! Environment, system, and explicit proxy selection.

use super::*;
use std::sync::Arc;

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

    pub(super) fn proxy_url_for_target(&self, route: &UrlTarget) -> HttpResult<Option<String>> {
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
