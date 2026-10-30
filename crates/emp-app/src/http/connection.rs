//! Socket lifecycle and streaming dispatch for a parsed HTTP request.

use crate::api::accounts::management_account_request;
use crate::api::catalog;
use crate::api::compact::compact_request;
use crate::api::integration::management_integration_request;
use crate::api::lifecycle::quit_request;
use crate::api::migration::management_migration_request;
use crate::api::quota::management_quota_request;
use crate::api::quota::serve_quota_events;
use crate::api::realtime::serve_realtime_call;
use crate::api::realtime::sideband::serve_realtime_sideband;
use crate::api::responses::ResponsesRequestResult;
use crate::api::responses::responses_request;
use crate::api::search::native_search_request;
use crate::api::websocket::serve_responses_websocket;
use crate::app::ServerState;
use crate::http::request::REQUEST_READ_TIMEOUT;
use crate::http::request::RequestMethod;
use crate::http::request::parse_request;
use crate::http::request::percent_decode;
use crate::http::request::read_request_head;
use crate::http::response::bad_request_response;
use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::http::routes::route_request_at;
use crate::services::connection_admission::ConnectionPermit;
use crate::util::system_now;
use std::net::Shutdown;
use std::net::TcpStream;

pub(crate) fn handle_connection(
    mut stream: TcpStream,
    state: &ServerState,
    mut request_permit: Option<ConnectionPermit>,
) {
    let mut observation = crate::services::observation::http::HttpObservation::new(state);
    if stream.set_nonblocking(false).is_err()
        || stream.set_nodelay(true).is_err()
        || stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).is_err()
    {
        observation.failure("socket_setup_failed");
        return;
    }
    let raw = match read_request_head(&mut stream) {
        Some(raw) => raw,
        None => {
            observation.write_response(&mut stream, &bad_request_response());
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    let mut stop_after_write = false;
    let observation_id = observation.request_id().map(str::to_owned);
    let Some(mut request) = parse_request(&raw.head) else {
        observation.write_response(&mut stream, &bad_request_response());
        let _ = stream.shutdown(Shutdown::Write);
        return;
    };
    request.observation_id = observation_id.as_deref();
    observation.received(request);
    let path = request.raw_path();
    if request.method == RequestMethod::Get && path == "/api/accounts/events" {
        // This long-lived management stream carries no Codex traffic. Keeping
        // its admission permit would make the UI block native restoration.
        let _ = request_permit.take();
    }
    // The management page carries a session; Codex does not.
    if (path.starts_with("/v1/models") || path.starts_with("/v1/responses"))
        && request.session_token().is_none()
    {
        crate::services::runtime::codex_contacted(state);
    }
    let update_request = path.starts_with("/api/updates/");
    let gated_mutation = (request.method == RequestMethod::Post && !update_request)
        || request.method == RequestMethod::Delete;
    let permit = if gated_mutation {
        state.updates.enter()
    } else {
        None
    };
    let response = if gated_mutation && permit.is_none() {
        Some(json_error_response(
            503,
            status_text(503),
            "EMP is installing an update. Please retry shortly.",
            Some("updating"),
            &[("Retry-After", "2")],
        ))
    } else {
        let now = system_now();
        match (request.method, path) {
            (RequestMethod::Post, "/v1/decisions") => Some(crate::api::decisions::request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post, path)
                if path.starts_with("/api/native/login/")
                    || path.starts_with("/api/accounts/login/") =>
            {
                Some(crate::api::native_login::request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            (RequestMethod::Post, "/api/updates/check" | "/api/updates/install") => Some(
                crate::api::updates::start(&mut stream, request, raw.body_prefix, state, now),
            ),
            (RequestMethod::Post, path)
                if path.starts_with("/api/accounts/") && path.ends_with("/models/refresh") =>
            {
                Some(management_account_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            (RequestMethod::Post, "/api/client-events") => {
                Some(crate::api::diagnostics::client_event(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            (RequestMethod::Post, "/api/usage/scan") => Some(crate::api::usage::scan(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post, "/api/runtime/scan") => {
                Some(crate::api::runtime::management_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            (RequestMethod::Post, "/api/quit") => {
                let (response, stop) =
                    quit_request(&mut stream, request, raw.body_prefix, state, now);
                stop_after_write = stop;
                Some(response)
            }

            (RequestMethod::Get, "/v1/responses")
                if request
                    .header("Upgrade")
                    .is_some_and(|value| value.eq_ignore_ascii_case("websocket")) =>
            {
                serve_responses_websocket(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                    observation.request_id(),
                );
                None
            }
            (RequestMethod::Post, "/v1/images/generations" | "/v1/images/edits") => {
                Some(crate::api::images::image_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            (RequestMethod::Post, "/v1/live") => Some(serve_realtime_call(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Get, path) if path.starts_with("/v1/live/") => {
                let call_id = percent_decode(&path["/v1/live/".len()..], false);
                serve_realtime_sideband(
                    &mut stream,
                    request,
                    &call_id,
                    raw.body_prefix,
                    state,
                    now,
                );
                None
            }
            (
                RequestMethod::Post,
                "/api/integration/enable"
                | "/api/integration/restore"
                | "/api/integration/reload"
                | "/api/integration/verify",
            ) => Some(management_integration_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post, "/v1/alpha/search") => Some(native_search_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post, "/api/accounts/import") => Some(management_account_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (
                RequestMethod::Post,
                "/api/migration/export"
                | "/api/migration/export/confirm"
                | "/api/migration/import"
                | "/api/migration/decode"
                | "/api/migration/preview"
                | "/api/migration/demo"
                | "/api/migration/lan/start"
                | "/api/migration/lan/status"
                | "/api/migration/lan/approve"
                | "/api/migration/lan/cancel"
                | "/api/migration/lan/receive",
            ) => Some(management_migration_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (
                RequestMethod::Post,
                "/api/providers/discover"
                | "/api/catalog/refresh"
                | "/api/catalog/context-preference"
                | "/api/models/metadata"
                | "/api/config"
                | "/api/language",
            ) => Some(catalog::management_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post, "/v1/responses/compact") => Some(compact_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
                observation.model.as_mut().expect("model HTTP observation"),
            )),
            (RequestMethod::Post, "/v1/responses") => {
                match responses_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                    observation.model.as_mut().expect("model HTTP observation"),
                ) {
                    ResponsesRequestResult::Buffered(response) => Some(response),
                    ResponsesRequestResult::Streamed => None,
                }
            }
            (RequestMethod::Get, "/api/accounts/events") => {
                serve_quota_events(&mut stream, request, state, now);
                None
            }
            (RequestMethod::Post, path)
                if path.starts_with("/api/accounts/")
                    && (path.ends_with("/quota") || path.ends_with("/quota-reset")) =>
            {
                Some(management_quota_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            (RequestMethod::Post, path)
                if path.starts_with("/api/providers/") && path.ends_with("/quota") =>
            {
                Some(crate::api::provider_quota::refresh(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    now,
                ))
            }
            _ => Some(route_request_at(request, state, now)),
        }
    };
    if let Some(response) = response {
        observation.write_response(&mut stream, &response);
    }
    let _ = stream.shutdown(Shutdown::Write);
    if stop_after_write {
        state.request_shutdown();
    }
}
