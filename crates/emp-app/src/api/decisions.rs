//! Jev uses OpenRouter Decisions, independently of the chat model catalog.

use crate::app::ServerState;
use crate::http::{
    auth::proxy_allowed,
    request::{Request, read_json_body},
    response::{body_error_response, json_error_response, response, status_text},
};
use emp_transport::HttpMethod;
use std::{collections::BTreeMap, net::TcpStream};
use url::Url;

pub(crate) fn request(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !proxy_allowed(request, state, now) {
        return json_error_response(
            401,
            status_text(401),
            "EMP gateway authentication is required",
            None,
            &[],
        );
    }
    let Some(provider_id) = request
        .header("X-EMP-Provider")
        .filter(|value| !value.is_empty() && request.header_count("X-EMP-Provider") == 1)
    else {
        return json_error_response(
            400,
            status_text(400),
            "X-EMP-Provider must name one configured OpenRouter service",
            None,
            &[],
        );
    };
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if !body["model"]
        .as_str()
        .is_some_and(|model| model.starts_with("typesafe/jev-"))
    {
        return json_error_response(
            400,
            status_text(400),
            "Decisions requires a typesafe/jev model",
            None,
            &[],
        );
    }
    let (endpoint, key) = {
        let Ok(config) = state.backend.configuration.read() else {
            return json_error_response(
                500,
                status_text(500),
                "configuration is unavailable",
                None,
                &[],
            );
        };
        let Some(provider) = config["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|provider| provider["id"] == provider_id && provider["enabled"] != false)
        else {
            return json_error_response(
                404,
                status_text(404),
                "OpenRouter service is not enabled",
                None,
                &[],
            );
        };
        let base = provider["base_url"]
            .as_str()
            .unwrap_or_default()
            .trim_end_matches('/');
        let endpoint =
            match Url::parse(&format!("{base}/")).and_then(|url| url.join("../alpha/decisions")) {
                Ok(url) => url.to_string(),
                Err(_) => {
                    return json_error_response(
                        400,
                        status_text(400),
                        "OpenRouter service URL is invalid",
                        None,
                        &[],
                    );
                }
            };
        let key = emp_state::provider_api_key(provider, &state.backend.configuration.vault);
        if key.is_empty() {
            return json_error_response(
                503,
                status_text(503),
                "OpenRouter service API key is unavailable",
                None,
                &[],
            );
        }
        (endpoint, key)
    };
    let headers = BTreeMap::from([
        ("Authorization".to_owned(), format!("Bearer {key}")),
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
    ]);
    let transport = &state.backend.transport;
    let upstream = match transport.runtime.block_on(transport.client.open(
        HttpMethod::Post,
        &endpoint,
        headers,
        Some(serde_json::to_vec(&body).expect("JSON request")),
        false,
    )) {
        Ok(upstream) => upstream,
        Err(_) => {
            return json_error_response(
                503,
                status_text(503),
                "Cannot connect to OpenRouter Decisions",
                Some("network_error"),
                &[],
            );
        }
    };
    let status = upstream.status();
    let content_type = upstream
        .header("content-type")
        .unwrap_or("application/json")
        .to_owned();
    let retry = upstream.header("retry-after").map(str::to_owned);
    let headers = retry
        .as_deref()
        .map(|value| vec![("Retry-After", value)])
        .unwrap_or_default();
    let raw = match transport
        .runtime
        .block_on(upstream.read_limited(emp_transport::MAX_PROXY_REQUEST_BYTES))
    {
        Ok(raw) => raw,
        Err(_) => {
            return json_error_response(
                502,
                status_text(502),
                "Decisions response is incomplete or too large",
                Some("protocol_error"),
                &[],
            );
        }
    };
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        &content_type,
        &raw,
        &headers,
    )
}
