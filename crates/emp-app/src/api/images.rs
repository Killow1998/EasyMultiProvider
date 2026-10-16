//! Standalone Codex image tools use their own JSON endpoints, outside Responses.
use crate::app::ServerState;
use crate::http::auth::{proxy_allowed, same_origin};
use crate::http::request::{Request, read_json_body};
use crate::http::response::{body_error_response, json_error_response, status_text};
use crate::services::native::images;
use std::collections::BTreeMap;
use std::net::TcpStream;

pub(crate) fn image_request(
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
    let mut incoming: BTreeMap<_, _> = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    if let Some(id) = request.observation_id {
        incoming.insert("X-EMP-Request-ID".into(), id.into());
    }
    let operation = if request.raw_path().ends_with("/edits") {
        "edits"
    } else {
        "generations"
    };
    crate::api::native_response::complete_response(images::execute(
        state, operation, &body, &incoming,
    ))
}
