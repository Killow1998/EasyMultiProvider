//! Api search.

use crate::VERSION;
use crate::app::ServerState;
use crate::http::auth::proxy_allowed;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::services::accounts::native_auth_document;
use crate::services::failures::request_router_error_response;
use crate::util::random_hex;
use emp_codex::account_auth_headers;
use emp_transport::HttpMethod;
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::TcpStream;
use std::path::Path;

pub(crate) fn native_search_request(
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
        return json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        );
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let config = match state.backend.configuration.config.lock() {
        Ok(config) => config.clone(),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    if config
        .get("subscription_search")
        .and_then(Value::as_object)
        .and_then(|search| search.get("enabled"))
        .and_then(Value::as_bool)
        != Some(true)
    {
        return json_error_response(
            403,
            status_text(403),
            "Subscription web search is disabled",
            Some("router_error"),
            &[],
        );
    }
    let mut headers = BTreeMap::from([
        ("Content-Type".to_owned(), "application/json".to_owned()),
        ("Accept".to_owned(), "application/json".to_owned()),
        ("User-Agent".to_owned(), format!("EMP/{VERSION}")),
    ]);
    let credentials = native_auth_document(&state.backend.accounts.native_auth_path)
        .and_then(|auth| account_auth_headers(&auth))
        .or_else(|| {
            config
                .get("accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|account| account.get("enabled") != Some(&Value::Bool(false)))
                .find_map(|account| {
                    let path = account.get("auth_file")?.as_str()?;
                    let auth = state
                        .backend
                        .configuration
                        .vault
                        .read_encrypted_json(Path::new(path))
                        .ok()?;
                    account_auth_headers(&auth)
                })
        });
    if let Some(credentials) = credentials {
        headers.extend(credentials);
    } else {
        for name in ["authorization", "chatgpt-account-id"] {
            if let Some(value) = request.header(name) {
                headers.insert(name.to_owned(), value.to_owned());
            }
        }
    }
    if let Ok(id) = random_hex(8) {
        headers.insert("X-EMP-Request-ID".to_owned(), id);
    }
    let base = config
        .get("codex_base_url")
        .and_then(Value::as_str)
        .unwrap_or("https://chatgpt.com/backend-api/codex")
        .trim_end_matches('/');
    let endpoint = if base.ends_with("/alpha/search") {
        base.to_owned()
    } else {
        format!("{base}/alpha/search")
    };
    let encoded = match serde_json::to_vec(&body) {
        Ok(body) => body,
        Err(_) => return request_router_error_response(400, "request body is invalid"),
    };
    let upstream =
        match state
            .backend
            .transport
            .runtime
            .block_on(state.backend.transport.client.open(
                HttpMethod::Post,
                &endpoint,
                headers,
                Some(encoded),
                false,
            )) {
            Ok(response) => response,
            Err(_) => {
                return json_error_response(
                    503,
                    status_text(503),
                    "Cannot connect to subscription web search",
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
    let raw = match state
        .backend
        .transport
        .runtime
        .block_on(upstream.read_limited(64 * 1024 * 1024))
    {
        Ok(raw) => raw,
        Err(_) => {
            return json_error_response(
                502,
                status_text(502),
                "native search response is too large or incomplete",
                Some("protocol_error"),
                &[],
            );
        }
    };
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        &content_type,
        &raw,
        &[],
    )
}
