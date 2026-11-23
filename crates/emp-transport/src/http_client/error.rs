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
    may_have_dispatched: bool,
}

impl HttpTransportError {
    pub(super) const fn new(kind: HttpTransportErrorKind) -> Self {
        Self {
            kind,
            may_have_dispatched: matches!(
                kind,
                HttpTransportErrorKind::ConnectTimeout
                    | HttpTransportErrorKind::ReadTimeout
                    | HttpTransportErrorKind::Network
                    | HttpTransportErrorKind::ResponseTooLarge
            ),
        }
    }

    pub const fn may_have_dispatched(self) -> bool {
        self.may_have_dispatched
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
    HttpTransportError {
        kind,
        may_have_dispatched: !error.is_connect() && !error.is_builder(),
    }
}

fn classify_source(error: &(dyn Error + 'static), proxy_connect: bool) -> HttpTransportErrorKind {
    let mut source = Some(error);
    let mut refused = false;
    while let Some(error) = source {
        if error.is::<rustls::Error>() {
            return HttpTransportErrorKind::TlsFailure;
        }
        let message = error.to_string();
        if matches!(
            message.as_str(),
            "dns error" | "error resolving for socks proxy"
        ) {
            return HttpTransportErrorKind::DnsFailure;
        }
        if proxy_connect && message == "SOCKS error: failed to create underlying connection" {
            return HttpTransportErrorKind::ProxyUnavailable;
        }
        if let Some(error) = error.downcast_ref::<io::Error>() {
            refused |= error.kind() == io::ErrorKind::ConnectionRefused;
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
