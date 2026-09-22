//! Api accounts.

use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::cross_origin_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use crate::services::accounts::import_account_state;
use std::net::TcpStream;

pub(crate) fn management_account_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
) -> Vec<u8> {
    if !same_origin(request, state.port) {
        return cross_origin_response("management session is required");
    }
    if !state
        .sessions
        .contains(request.session_cookie().as_deref(), now)
    {
        return unauthorized_response();
    }
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    if let Some(id) = request
        .raw_path()
        .strip_prefix("/api/accounts/")
        .and_then(|path| path.strip_suffix("/models/refresh"))
    {
        let id = crate::http::request::percent_decode(id, false);
        return match crate::services::account_catalog::refresh(state, &id) {
            Ok(payload) => response(
                "HTTP/1.1 200 OK",
                "application/json",
                &serde_json::to_vec(&payload).expect("subscription models"),
                &[],
            ),
            Err(response) => response,
        };
    }
    match import_account_state(state, &body) {
        Ok(account) => {
            let body = serde_json::to_vec(&serde_json::json!({"account":account})).unwrap();
            response("HTTP/1.1 200 OK", "application/json", &body, &[])
        }
        Err(error) => json_error_response(400, status_text(400), &error, None, &[]),
    }
}
