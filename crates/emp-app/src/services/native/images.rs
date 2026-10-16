//! Native image forwarding shares account authentication, never response projection.
use super::resolve_headers;
use crate::app::ServerState;
use crate::services::request_outcome::RequestOutcome;
use emp_router::native_http::{NativeCompleteResponse, NativeHttpError};
use emp_transport::{HttpMethod, HttpTransportErrorKind};
use serde_json::{Value, json};
use std::collections::BTreeMap;

mod routing;
pub(crate) use routing::ImageRoutes;

pub(crate) fn execute(
    state: &ServerState,
    operation: &str,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<NativeCompleteResponse, NativeHttpError> {
    let route = state
        .backend
        .transport
        .image_routes
        .resolve(state, body, incoming)?;
    let base = route
        .provider
        .value()
        .get("base_url")
        .and_then(Value::as_str)
        .ok_or_else(|| NativeHttpError::router(503, "image backend is not configured"))?
        .trim_end_matches('/');
    let base = base.strip_suffix("/responses").unwrap_or(base);
    let endpoint = format!("{base}/images/{operation}");
    let encoded = serde_json::to_vec(body)
        .map_err(|_| NativeHttpError::router(400, "image request is invalid"))?;
    let mut usage = RequestOutcome::new(
        state,
        &route,
        body,
        incoming,
        None,
        if operation == "edits" {
            "image_edit"
        } else {
            "image_generation"
        },
    );
    let _activity =
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                &route,
            ));
    let result = (|| {
        let mut auth_headers = resolve_headers(state, &route, incoming, false, false)?;
        for attempt in 0..2 {
            usage.owner(&emp_state::usage::account_owner(&auth_headers));
            usage.dispatch();
            let runtime = &state.backend.transport.runtime;
            let upstream = runtime
                .block_on(state.backend.transport.client.open(
                    HttpMethod::Post,
                    &endpoint,
                    auth_headers.clone(),
                    Some(encoded.clone()),
                    false,
                ))
                .map_err(|error| transport_error(error.kind()))?;
            let status = upstream.status();
            let content_type = upstream
                .header("content-type")
                .unwrap_or("application/json")
                .to_owned();
            let values = upstream
                .headers()
                .map(|(name, value)| (name.to_owned(), json!(value)))
                .collect::<serde_json::Map<_, _>>();
            let mut headers: BTreeMap<String, String> =
                emp_router::native_metadata::native_response_headers(
                    &json!({"headers":values}),
                    &route.requested_model,
                    &route.upstream_model,
                )
                .into_iter()
                .filter_map(|(name, value)| value.as_str().map(|value| (name, value.to_owned())))
                .collect();
            for name in ["x-codex-imagegen-request-id", "retry-after"] {
                if let Some(value) = upstream.header(name) {
                    headers.insert(name.to_owned(), value.to_owned());
                }
            }
            let receipt_id = headers
                .get("x-codex-imagegen-request-id")
                .or_else(|| headers.get("x-request-id"))
                .or_else(|| incoming.get("X-EMP-Request-ID"))
                .cloned();
            let raw = runtime
                .block_on(upstream.read_limited(64 * 1024 * 1024))
                .map_err(|error| transport_error(error.kind()))?;
            if status == 401
                && attempt == 0
                && route
                    .provider
                    .value()
                    .get("auth_mode")
                    .and_then(Value::as_str)
                    == Some("account")
            {
                let refreshed = resolve_headers(state, &route, incoming, false, true);
                usage.native_retry(emp_router::native_http::NativeRetryDecision {
                    reason: emp_router::native_http::NativeRetryReason::AccountRefresh,
                    status,
                    retry: refreshed.is_ok(),
                });
                if let Ok(headers) = refreshed {
                    auth_headers = headers;
                    continue;
                }
            }
            usage.http_status(status);
            if let Ok(mut value) = serde_json::from_slice::<Value>(&raw) {
                // Images have no Responses ID. Correlate accounting and call
                // receipts without changing the image body returned to Codex.
                if let Some(object) = value.as_object_mut()
                    && !object.contains_key("id")
                    && let Some(id) = receipt_id
                {
                    object.insert("id".into(), json!(id));
                }
                usage.observe(&value);
            }
            return Ok(NativeCompleteResponse {
                status,
                content_type,
                body: raw,
                headers,
                observation: Default::default(),
            });
        }
        unreachable!("image authentication retries are bounded")
    })();
    if let Err(error) = &result {
        usage.native_error(error);
    }
    result
}

fn transport_error(kind: HttpTransportErrorKind) -> NativeHttpError {
    let (status, code) = match kind {
        HttpTransportErrorKind::DnsFailure => (503, "dns_failure"),
        HttpTransportErrorKind::TlsFailure => (502, "tls_failure"),
        HttpTransportErrorKind::ProxyUnavailable => (503, "proxy_unavailable"),
        HttpTransportErrorKind::ConnectTimeout | HttpTransportErrorKind::ReadTimeout => {
            (504, "upstream_timeout")
        }
        HttpTransportErrorKind::ResponseTooLarge => (502, "image_response_too_large"),
        _ => (503, "network_error"),
    };
    NativeHttpError {
        status,
        headers: BTreeMap::new(),
        body: json!({"error":{
        "origin":"transport", "code":code, "type":"network_error",
        "message":format!("[EMP connection] Image request failed: {code}")}}),
    }
}
