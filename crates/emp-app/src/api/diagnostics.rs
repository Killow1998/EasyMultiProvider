//! Existing diagnostic chart and typed browser event APIs.
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::{Request, read_json_body};
use crate::http::response::{
    body_error_response, cross_origin_response, json_error_response, response, status_text,
    unauthorized_response,
};
use emp_state::diagnostics::schema;
use serde_json::{Value, json};
use std::net::TcpStream;

pub(crate) fn read(state: &ServerState) -> Vec<u8> {
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(&state.backend.diagnostics.snapshot()).expect("diagnostic snapshot"),
        &[],
    )
}
pub(crate) fn client_event(
    stream: &mut TcpStream,
    request: Request<'_>,
    prefix: Vec<u8>,
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
    let body = match read_json_body(stream, request, prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    let kind = body["kind"].as_str().unwrap_or("");
    let phase = body["phase"].as_str().unwrap_or("");
    if !["integration_load", "window_error", "unhandled_rejection"].contains(&kind)
        || !["render_completed", "failed"].contains(&phase)
    {
        return json_error_response(
            400,
            status_text(400),
            "invalid web client diagnostic event",
            None,
            &[],
        );
    }
    let failure = body.get("failure_class").and_then(Value::as_str).unwrap_or(
        if body.get("failure_class").is_none() {
            "none"
        } else {
            "unknown"
        },
    );
    let failure = if [
        "none",
        "network",
        "http_error",
        "missing_element",
        "type_error",
        "syntax_error",
        "unknown",
    ]
    .contains(&failure)
    {
        failure
    } else {
        "unknown"
    };
    let mut fields = json!({"kind":kind,"phase":phase,"failure_class":failure});
    let page = schema::id(&body["page_id"]);
    if !page.is_empty() {
        fields["page_id"] = json!(page);
    }
    for (field, maximum) in [
        ("duration_ms", 3_600_000),
        ("http_status", 599),
        ("line", 10_000_000),
        ("column", 10_000_000),
    ] {
        let value = schema::integer(&body[field], maximum);
        if !value.is_null() {
            fields[field] = value;
        }
    }
    state.backend.diagnostics.journal.event(
        if phase == "failed" { "warning" } else { "info" },
        "web_client_phase",
        &fields,
    );
    response("HTTP/1.1 200 OK", "application/json", b"{\"ok\":true}", &[])
}
