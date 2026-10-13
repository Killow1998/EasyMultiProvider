//! One content-free receipt per downstream turn. Unknown measurements stay null.
use serde::Serialize;

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContinuationReason {
    UnsupportedDialect,
    ScopeMismatch,
    RouteMismatch,
    IdentityMismatch,
    PriorResponseMismatch,
    UpstreamDisconnected,
    UpstreamRecoveryRequired,
    IncrementalFallbackUnavailable,
    PrefillRejected,
    PrefillInvalidResponse,
}

#[derive(Default, Serialize)]
pub(crate) struct NativeContinuation {
    pub(crate) reason: Option<ContinuationReason>,
    pub(crate) has_prior: bool,
    pub(crate) local_prior_matches: Option<bool>,
    pub(crate) upstream_connection_available: Option<bool>,
    pub(crate) scope_matches: Option<bool>,
    pub(crate) route_matches: Option<bool>,
    pub(crate) identity_matches: Option<bool>,
    pub(crate) connect_attempts: u32,
    pub(crate) connections_opened: u32,
    pub(crate) connection_reused: Option<bool>,
    pub(crate) handshake_status: Option<u16>,
    pub(crate) peer_close_code: Option<u16>,
    pub(crate) http_fallback: bool,
    pub(crate) prefill_requests: u32,
    pub(crate) prefill_application_bytes: u64,
    pub(crate) downstream_decoded_bytes: Option<u64>,
    pub(crate) upstream_decoded_bytes_sent: Option<u64>,
    pub(crate) upstream_encoded_bytes_sent: Option<u64>,
    pub(crate) upstream_decoded_bytes_received: Option<u64>,
    pub(crate) upstream_encoded_bytes_received: Option<u64>,
    pub(crate) permessage_deflate: Option<bool>,
}

impl NativeContinuation {
    pub(crate) fn traffic(
        &mut self,
        before: emp_transport::WebSocketTraffic,
        after: emp_transport::WebSocketTraffic,
        compression: bool,
    ) {
        // Text application payloads only, excluding WS framing/control,
        // handshakes, TLS, proxies and IP overhead.
        self.upstream_decoded_bytes_sent = delta(before.decoded_sent, after.decoded_sent);
        self.upstream_encoded_bytes_sent = delta(before.encoded_sent, after.encoded_sent);
        self.upstream_decoded_bytes_received =
            delta(before.decoded_received, after.decoded_received);
        self.upstream_encoded_bytes_received =
            delta(before.encoded_received, after.encoded_received);
        self.permessage_deflate = Some(compression);
    }
}
fn delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    after?.checked_sub(before?)
}
