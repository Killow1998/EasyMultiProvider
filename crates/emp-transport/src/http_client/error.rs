//! Reduce private transport errors to content-free, actionable categories.
use crate::http_policy::HttpClientPolicyError;
use std::{error::Error, fmt, io};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpTransportErrorKind {
    InvalidRequest,
    ClientBuild,
    ConnectTimeout,
    ReadTimeout,
    ResponseTooLarge,
    Network,
    DnsFailure,
    TlsFailure,
    ProxyUnavailable,
    RedirectDisabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpTransportError {
    kind: HttpTransportErrorKind,
}

impl HttpTransportError {
    pub(super) const fn new(kind: HttpTransportErrorKind) -> Self {
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
            HttpTransportErrorKind::DnsFailure => "connection host name could not be resolved",
            HttpTransportErrorKind::TlsFailure => {
                "secure connection verification or handshake failed"
            }
            HttpTransportErrorKind::ProxyUnavailable => "configured proxy could not be reached",
            HttpTransportErrorKind::RedirectDisabled => "upstream redirects are disabled",
        })
    }
}

impl Error for HttpTransportError {}

impl From<HttpClientPolicyError> for HttpTransportError {
    fn from(_: HttpClientPolicyError) -> Self {
        Self::new(HttpTransportErrorKind::InvalidRequest)
    }
}

pub(super) fn map_send_error(error: reqwest::Error, proxied: bool) -> HttpTransportError {
    let kind = if error.is_timeout() {
        HttpTransportErrorKind::ConnectTimeout
    } else {
        classify_source(&error, proxied && error.is_connect())
    };
    HttpTransportError::new(kind)
}

fn classify_source(error: &(dyn Error + 'static), proxy_connect: bool) -> HttpTransportErrorKind {
    let mut source = Some(error);
    let mut refused = false;
    while let Some(error) = source {
        if error.is::<rustls::Error>() {
            return HttpTransportErrorKind::TlsFailure;
        }
        // hyper-util's DNS wrapper has no public discriminator. Match only its
        // exact fixed message, never OS/localized text or arbitrary URL content.
        let message = error.to_string();
        if matches!(
            message.as_str(),
            "dns error" | "error resolving for socks proxy"
        ) {
            return HttpTransportErrorKind::DnsFailure;
        }
        // hyper-util's SOCKS Inner error does not expose source(). This fixed
        // marker denotes failure to reach the proxy, not its upstream reply.
        if proxy_connect && message == "SOCKS error: failed to create underlying connection" {
            return HttpTransportErrorKind::ProxyUnavailable;
        }
        if let Some(error) = error.downcast_ref::<io::Error>() {
            refused |= error.kind() == io::ErrorKind::ConnectionRefused;
            // io::Error::source can skip the contained error itself.
            if let Some(inner) = error.get_ref() {
                source = Some(inner);
                continue;
            }
        }
        source = error.source();
    }
    if proxy_connect && refused {
        HttpTransportErrorKind::ProxyUnavailable
    } else {
        HttpTransportErrorKind::Network
    }
}
