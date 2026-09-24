//! Native upstream HTTP call creation and response validation.

use super::multipart::read_realtime_call;
use super::{
    MAX_REALTIME_RESPONSE_BYTES, REALTIME_TIMEOUT, RealtimeError, RealtimeResponse, header,
    valid_call_id,
};
use crate::VERSION;
use crate::app::ServerState;
use crate::http::auth::{proxy_allowed, same_origin};
use crate::http::request::Request;
use crate::http::response::response;
use crate::services::accounts::native_auth_document;
use emp_codex::account_auth_headers;
use emp_transport::{HttpMethod, HttpTransportErrorKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpStream;

pub(crate) fn serve_realtime_call(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        if status == 401 && native_headers(state).is_none() {
            return RealtimeError::new(
                401,
                "native_subscription_unavailable",
                "A current native ChatGPT subscription login is required for Codex Voice",
            )
            .wire_response();
        }
        return RealtimeError::new(
            status,
            "realtime_caller_unauthorized",
            "Caller authentication is required for Codex Voice",
        )
        .wire_response();
    }
    let incoming = incoming_headers(request);
    let mut body_stream = stream;
    let call = match read_realtime_call(&incoming, body_prefix, &mut body_stream) {
        Ok(call) => call,
        Err(error) => return error.wire_response(),
    };
    let Some(mut headers) = native_headers(state) else {
        return RealtimeError::new(
            401,
            "native_subscription_unavailable",
            "A current native ChatGPT subscription login is required for Codex Voice",
        )
        .wire_response();
    };
    let forwarded = match safe_forwarded_headers(&incoming) {
        Ok(headers) => headers,
        Err(error) => return error.wire_response(),
    };
    headers.extend(forwarded);
    headers.insert("Content-Type".to_owned(), "application/json".to_owned());
    headers.insert("Accept".to_owned(), "application/sdp".to_owned());
    headers.insert("User-Agent".to_owned(), format!("EMP/{VERSION}"));
    let base_url = match state.backend.configuration.config.lock() {
        Ok(config) => config
            .get("codex_base_url")
            .and_then(Value::as_str)
            .unwrap_or("https://chatgpt.com/backend-api/codex")
            .to_owned(),
        Err(_) => {
            return RealtimeError::new(
                503,
                "native_realtime_unavailable",
                "Native subscription backend is not configured for realtime calls",
            )
            .wire_response();
        }
    };
    let Some(endpoint) = upstream_endpoint(&base_url) else {
        return RealtimeError::new(
            503,
            "native_realtime_unavailable",
            "Native subscription backend is not configured for realtime calls",
        )
        .wire_response();
    };
    let request_body = match serde_json::to_vec(&json!({"sdp":call.sdp,"session":call.session})) {
        Ok(body) => body,
        Err(_) => {
            return RealtimeError::new(
                400,
                "realtime_invalid_request",
                "Realtime request could not be encoded",
            )
            .wire_response();
        }
    };
    let result = state.backend.transport.runtime.block_on(async {
        tokio::time::timeout(REALTIME_TIMEOUT, async {
            let upstream = state
                .backend
                .transport
                .client
                .open_status(
                    HttpMethod::Post,
                    &endpoint,
                    headers,
                    Some(request_body),
                    false,
                )
                .await
                .map_err(map_http_error)?;
            let status = upstream.status();
            let content_type = upstream
                .header("content-type")
                .and_then(safe_upstream_header)
                .unwrap_or_else(|| {
                    if (200..300).contains(&status) {
                        "application/sdp".to_owned()
                    } else {
                        "application/json".to_owned()
                    }
                });
            let location = upstream.header("location").and_then(safe_upstream_header);
            let body = upstream
                .read_limited(MAX_REALTIME_RESPONSE_BYTES)
                .await
                .map_err(|error| {
                    if error.kind() == HttpTransportErrorKind::ResponseTooLarge {
                        RealtimeError::new(
                            502,
                            "realtime_upstream_invalid",
                            "Native realtime response exceeds the allowed size",
                        )
                    } else {
                        map_http_error(error)
                    }
                })?;
            let result = RealtimeResponse {
                status,
                content_type,
                body,
                location,
            };
            validate_upstream_response(result)
        })
        .await
    });
    let result = match result {
        Err(_) => Err(RealtimeError::new(
            504,
            "native_realtime_timeout",
            "Native realtime call creation timed out",
        )),
        Ok(result) => result,
    };
    match result {
        Ok(result) => wire_upstream_response(result),
        Err(error) => error.wire_response(),
    }
}

pub(super) fn incoming_headers(request: Request<'_>) -> BTreeMap<String, String> {
    request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect()
}

pub(super) fn native_headers(state: &ServerState) -> Option<BTreeMap<String, String>> {
    let auth = native_auth_document(&state.backend.accounts.native_auth_path)?;
    let auth = emp_state::validate_auth_json(&auth).ok()?;
    account_auth_headers(&auth)
}

pub(super) fn safe_forwarded_headers(
    incoming: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, RealtimeError> {
    const FORWARDED: [(&str, &str); 8] = [
        ("openai-alpha", "OpenAI-Alpha"),
        ("originator", "originator"),
        ("session-id", "session-id"),
        ("thread-id", "thread-id"),
        ("x-session-id", "x-session-id"),
        ("x-codex-installation-id", "x-codex-installation-id"),
        ("x-codex-turn-metadata", "x-codex-turn-metadata"),
        ("x-oai-attestation", "x-oai-attestation"),
    ];
    let mut result = BTreeMap::new();
    for (source, target) in FORWARDED {
        let Some(value) = header(incoming, source) else {
            continue;
        };
        if value.is_empty() || value.len() > 8192 {
            continue;
        }
        if !value.is_ascii() || value.bytes().any(|byte| byte < 0x20 || byte == 0x7f) {
            return Err(RealtimeError::new(
                400,
                "realtime_invalid_header",
                "Realtime request contains an invalid forwarded header",
            ));
        }
        result.insert(target.to_owned(), value.to_owned());
    }
    Ok(result)
}

fn upstream_endpoint(base: &str) -> Option<String> {
    let (scheme, rest) = base.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https")
        || rest.is_empty()
        || rest.contains(['?', '#'])
    {
        return None;
    }
    let authority = rest.split('/').next()?;
    if authority.is_empty()
        || authority.contains('@')
        || authority.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return None;
    }
    Some(format!(
        "{}/realtime/calls?intent=quicksilver&architecture=avas",
        base.trim_end_matches('/')
    ))
}

fn safe_upstream_header(value: &str) -> Option<String> {
    (!value.is_empty()
        && value.len() <= 2048
        && value.is_ascii()
        && !value.bytes().any(|byte| byte < 0x20 || byte == 0x7f))
    .then(|| value.to_owned())
}

fn location_has_call_id(location: &str) -> bool {
    let path = if let Some(scheme_end) = location.find("://") {
        let authority_and_path = &location[scheme_end + 3..];
        authority_and_path
            .find('/')
            .map(|offset| &authority_and_path[offset..])
            .unwrap_or_default()
    } else {
        location
    };
    path.split(['?', '#'])
        .next()
        .unwrap_or_default()
        .split('/')
        .any(valid_call_id)
}

fn map_http_error(error: emp_transport::HttpTransportError) -> RealtimeError {
    match error.kind() {
        HttpTransportErrorKind::ConnectTimeout | HttpTransportErrorKind::ReadTimeout => {
            RealtimeError::new(
                504,
                "native_realtime_timeout",
                "Native realtime call creation timed out",
            )
        }
        _ => RealtimeError::new(
            502,
            "native_realtime_transport_error",
            "Native realtime call creation could not reach the subscription backend",
        ),
    }
}

fn validate_upstream_response(
    mut upstream: RealtimeResponse,
) -> Result<RealtimeResponse, RealtimeError> {
    if (200..300).contains(&upstream.status) {
        if upstream
            .location
            .as_deref()
            .is_none_or(|location| !location_has_call_id(location))
        {
            return Err(RealtimeError::new(
                502,
                "realtime_upstream_invalid",
                "Native realtime response is missing a valid call Location",
            ));
        }
        if upstream.body.is_empty() {
            return Err(RealtimeError::new(
                502,
                "realtime_upstream_invalid",
                "Native realtime response is missing the SDP answer",
            ));
        }
    } else if upstream.body.is_empty() && matches!(upstream.status, 401 | 403 | 404 | 405 | 501) {
        let code = if matches!(upstream.status, 401 | 403) {
            "native_subscription_auth_failed"
        } else {
            "native_realtime_unsupported"
        };
        let message = if matches!(upstream.status, 401 | 403) {
            "Native subscription authentication failed for Codex Voice"
        } else {
            "The native subscription backend does not support Codex Voice"
        };
        let media = upstream
            .content_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        upstream.body = if media == "application/json" || media.ends_with("+json") {
            serde_json::to_vec(&json!({"error":{"code":code,"message":message}}))
                .expect("realtime error JSON")
        } else {
            format!("{code}: {message}").into_bytes()
        };
    }
    Ok(upstream)
}

fn wire_upstream_response(upstream: RealtimeResponse) -> Vec<u8> {
    let reason = upstream_reason(upstream.status);
    let location = upstream
        .location
        .as_deref()
        .map(|location| [("Location", location)]);
    response(
        &format!("HTTP/1.1 {} {reason}", upstream.status),
        &upstream.content_type,
        &upstream.body,
        location.as_ref().map_or(&[], |headers| headers.as_slice()),
    )
}

fn upstream_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        418 => "I'm a Teapot",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Error",
    }
}
