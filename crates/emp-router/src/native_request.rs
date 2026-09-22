//! Native request headers after the application resolves the credential owner.
//! This layer never reads credentials or treats the caller's bearer as an
//! account selection; the account/implicit-native decision is request-local.

use crate::{EMP_VERSION, RouterError, RouterErrorKind};
use emp_transport::FailureClass;
use std::collections::BTreeMap;

/// Credentials have already been resolved by the owning application boundary.
/// `Implicit(None)` preserves Python's incoming-auth fallback when no native
/// login can be read; `Implicit(Some(empty))` deliberately does not fall back.
pub enum NativeAuth<'a> {
    Forward,
    Implicit(Option<&'a BTreeMap<String, String>>),
    Account(&'a BTreeMap<String, String>),
}

const CONTEXT_HEADERS: &[(&str, &str)] = &[
    ("openai-beta", "OpenAI-Beta"),
    ("originator", "originator"),
    ("session-id", "session-id"),
    ("thread-id", "thread-id"),
    ("x-client-request-id", "x-client-request-id"),
    ("x-codex-beta-features", "x-codex-beta-features"),
    ("x-codex-installation-id", "x-codex-installation-id"),
    ("x-codex-parent-thread-id", "x-codex-parent-thread-id"),
    ("x-codex-routing-hint", "x-codex-routing-hint"),
    ("x-codex-turn-state", "x-codex-turn-state"),
    ("x-codex-turn-metadata", "x-codex-turn-metadata"),
    ("x-codex-window-id", "x-codex-window-id"),
    ("x-oai-attestation", "x-oai-attestation"),
    ("x-openai-memgen-request", "x-openai-memgen-request"),
    (
        "x-openai-internal-codex-responses-lite",
        "x-openai-internal-codex-responses-lite",
    ),
    ("x-openai-subagent", "x-openai-subagent"),
    (
        "x-responsesapi-include-timing-metrics",
        "x-responsesapi-include-timing-metrics",
    ),
];

/// Project Python's native allowlist and preserve resolved credential precedence.
pub fn request_headers(
    auth: NativeAuth<'_>,
    incoming: &BTreeMap<String, String>,
    stream: bool,
) -> Result<BTreeMap<String, String>, RouterError> {
    let mut headers = BTreeMap::from([
        ("Content-Type".to_owned(), "application/json".to_owned()),
        (
            "Accept".to_owned(),
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            }
            .to_owned(),
        ),
        ("User-Agent".to_owned(), format!("EMP/{EMP_VERSION}")),
    ]);
    if let Some(id) = incoming.get("X-EMP-Request-ID")
        && id.len() == 16
        && id
            .bytes()
            .all(|ch| ch.is_ascii_digit() || (b'a'..=b'f').contains(&ch))
    {
        headers.insert("X-EMP-Request-ID".to_owned(), id.clone());
    }
    let lower: BTreeMap<_, _> = incoming
        .iter()
        .map(|(name, value)| (name.to_lowercase(), value))
        .collect();
    match auth {
        NativeAuth::Account(selected) | NativeAuth::Implicit(Some(selected)) => {
            headers.extend(selected.clone())
        }
        NativeAuth::Forward | NativeAuth::Implicit(None) => {
            for (source, target) in [
                ("authorization", "Authorization"),
                ("chatgpt-account-id", "chatgpt-account-id"),
            ] {
                if let Some(value) = lower.get(source).filter(|value| !value.is_empty()) {
                    headers.insert(target.to_owned(), (*value).clone());
                }
            }
            if !headers.contains_key("Authorization") {
                return Err(RouterError::new(
                    RouterErrorKind::MissingCredential,
                    401,
                    FailureClass::RouterError,
                    None,
                    None,
                    if matches!(auth, NativeAuth::Forward) {
                        "forward provider requires an incoming Authorization header"
                    } else {
                        "native Codex login credentials are unavailable"
                    },
                ));
            }
        }
    }
    for (source, target) in CONTEXT_HEADERS {
        if let Some(value) = lower.get(*source).filter(|value| !value.is_empty()) {
            headers.insert((*target).to_owned(), (*value).clone());
        }
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_selection_keeps_context_without_forwarding_caller_credentials() {
        let selected = BTreeMap::from([
            ("Authorization".to_owned(), "Bearer selected".to_owned()),
            ("chatgpt-account-id".to_owned(), "selected-owner".to_owned()),
        ]);
        let incoming = BTreeMap::from([
            ("Authorization".to_owned(), "Bearer caller".to_owned()),
            ("chatgpt-account-id".to_owned(), "caller-owner".to_owned()),
            ("Thread-ID".to_owned(), "thread".to_owned()),
            ("Cookie".to_owned(), "private".to_owned()),
        ]);
        let headers = request_headers(NativeAuth::Account(&selected), &incoming, true).unwrap();
        assert_eq!(headers["Authorization"], "Bearer selected");
        assert_eq!(headers["chatgpt-account-id"], "selected-owner");
        assert_eq!(headers["thread-id"], "thread");
        assert_eq!(headers["Accept"], "text/event-stream");
        assert!(!headers.contains_key("Cookie"));
    }
}
