//! HTTP method/path dispatch. Domain logic lives in services.

use crate::api::accounts::management_account_request;
use crate::api::catalog;
use crate::api::compact::compact_request;
use crate::api::inspection;
use crate::api::integration::{management_integration_request, read_integration_request};
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
use crate::http::auth::BOOTSTRAP_HEADER;
use crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE;
use crate::http::auth::same_origin;
use crate::http::request::REQUEST_READ_TIMEOUT;
use crate::http::request::Request;
use crate::http::request::RequestMethod;
use crate::http::request::parse_request;
use crate::http::request::percent_decode;
use crate::http::request::query_values;
use crate::http::request::read_request_head;
use crate::http::response::bad_request_response;
use crate::http::response::cross_origin_response;
use crate::http::response::health_response;
use crate::http::response::json_error_response;
use crate::http::response::not_found_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::http::response::unauthorized_response;
use crate::services::accounts::accounts_snapshot;
use crate::services::accounts::delete_account_state;
use crate::services::connection_admission::ConnectionPermit;
use crate::services::quota::QuotaHistoryResponseError;
use crate::services::quota::quota_history_response;
use crate::util::system_now;
use crate::web::ui_response;
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
        match Some(request) {
            Some(request)
                if request.method == RequestMethod::Post
                    && matches!(
                        request.raw_path(),
                        "/api/updates/check" | "/api/updates/install"
                    ) =>
            {
                Some(crate::api::updates::start(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path().starts_with("/api/accounts/")
                    && request.raw_path().ends_with("/models/refresh") =>
            {
                Some(management_account_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/api/client-events" =>
            {
                Some(crate::api::diagnostics::client_event(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/api/usage/scan" =>
            {
                Some(crate::api::usage::scan(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/api/runtime/scan" =>
            {
                Some(crate::api::runtime::management_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post && request.raw_path() == "/api/quit" =>
            {
                let (response, stop) =
                    quit_request(&mut stream, request, raw.body_prefix, state, system_now());
                stop_after_write = stop;
                Some(response)
            }

            Some(request)
                if request.method == RequestMethod::Get
                    && request.raw_path() == "/v1/responses"
                    && request
                        .header("Upgrade")
                        .is_some_and(|value| value.eq_ignore_ascii_case("websocket")) =>
            {
                serve_responses_websocket(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                    observation.request_id(),
                );
                None
            }
            Some(request)
                if request.method == RequestMethod::Post && request.raw_path() == "/v1/live" =>
            {
                Some(serve_realtime_call(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Get
                    && request.raw_path().starts_with("/v1/live/") =>
            {
                let raw_path = request.raw_path();
                let call_id = percent_decode(&raw_path["/v1/live/".len()..], false);
                serve_realtime_sideband(
                    &mut stream,
                    request,
                    &call_id,
                    raw.body_prefix,
                    state,
                    system_now(),
                );
                None
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && matches!(
                        request.raw_path(),
                        "/api/integration/enable"
                            | "/api/integration/restore"
                            | "/api/integration/reload"
                            | "/api/integration/verify"
                    ) =>
            {
                Some(management_integration_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                    &mut stop_after_write,
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/v1/alpha/search" =>
            {
                Some(native_search_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/api/accounts/import" =>
            {
                Some(management_account_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && matches!(
                        request.raw_path(),
                        "/api/migration/export"
                            | "/api/migration/export/confirm"
                            | "/api/migration/import"
                    ) =>
            {
                Some(management_migration_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && matches!(
                        request.raw_path(),
                        "/api/providers/discover"
                            | "/api/catalog/refresh"
                            | "/api/catalog/context-preference"
                            | "/api/models/metadata"
                            | "/api/config"
                    ) =>
            {
                Some(catalog::management_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/v1/responses/compact" =>
            {
                Some(compact_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                    observation.model.as_mut().expect("model HTTP observation"),
                ))
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path() == "/v1/responses" =>
            {
                match responses_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                    observation.model.as_mut().expect("model HTTP observation"),
                ) {
                    ResponsesRequestResult::Buffered(response) => Some(response),
                    ResponsesRequestResult::Streamed => None,
                }
            }
            Some(request)
                if request.method == RequestMethod::Get
                    && request.raw_path() == "/api/accounts/events" =>
            {
                serve_quota_events(&mut stream, request, state, system_now());
                None
            }
            Some(request)
                if request.method == RequestMethod::Post
                    && request.raw_path().starts_with("/api/accounts/")
                    && (request.raw_path().ends_with("/quota")
                        || request.raw_path().ends_with("/quota-reset")) =>
            {
                Some(management_quota_request(
                    &mut stream,
                    request,
                    raw.body_prefix,
                    state,
                    system_now(),
                ))
            }
            Some(request) => Some(route_request(request, state)),
            None => Some(bad_request_response()),
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

fn route_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    route_request_at(request, state, system_now())
}

pub(crate) fn route_request_at(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let path = request.raw_path();
    let same_origin = same_origin(request, state.port);
    if request.method == RequestMethod::Get
        && let Some(bytes) = crate::web::asset_response(path)
    {
        return if same_origin {
            bytes
        } else {
            cross_origin_response("cross-origin Web UI request rejected")
        };
    }
    if request.method == RequestMethod::Get
        && (path == "/v1/models" || path.starts_with("/v1/models/"))
    {
        if !same_origin {
            return cross_origin_response("cross-origin model catalog request rejected");
        }
        return catalog::models_request(request, state);
    }
    if path == "/healthz" {
        if !same_origin {
            return cross_origin_response("cross-origin health request rejected");
        }
        return health_response();
    }
    if request.method == RequestMethod::Get && path == "/api/updates" {
        return crate::api::updates::read(request, state, now);
    }

    if path == "/" || path == "/index.html" {
        if !same_origin {
            return cross_origin_response("cross-origin Web UI request rejected");
        }
        // The page carries no secrets; its script exchanges the bootstrap
        // token for an origin-scoped session sent as a request header.
        return ui_response();
    }

    if request.method == RequestMethod::Post && path == "/api/session" {
        if !same_origin {
            return cross_origin_response("management session is required");
        }
        return bootstrap_session(request, state, now);
    }

    if path.starts_with("/api/") {
        if !same_origin {
            return cross_origin_response("management session is required");
        }
        let supplied_session = request.session_token();
        if state.sessions.contains(supplied_session.as_deref(), now) {
            if request.method == RequestMethod::Get && path == "/api/support-report" {
                return crate::api::support_report::read(state);
            }
            if request.method == RequestMethod::Get && path == "/api/diagnostics" {
                return crate::api::diagnostics::read(state);
            }
            if request.method == RequestMethod::Get && path == "/api/usage" {
                return crate::api::usage::read(request, state);
            }
            if request.method == RequestMethod::Get && path == "/api/runtime/claude-cli" {
                return crate::api::runtime::claude_cli_availability_request();
            }
            if request.method == RequestMethod::Get
                && matches!(
                    path,
                    "/api/models/vision-test-image"
                        | "/api/models/audio-test-sound"
                        | "/api/request-limits"
                        | "/api/capabilities"
                )
            {
                return inspection::read_request(request, state);
            }
            if request.method == RequestMethod::Get
                && (path == "/api/config"
                    || (path.starts_with("/api/accounts/") && path.ends_with("/models")))
            {
                return catalog::read_management_request(request, state);
            }
            if request.method == RequestMethod::Get && path == "/api/accounts" {
                let Some(snapshot) = accounts_snapshot(state) else {
                    return json_error_response(
                        500,
                        status_text(500),
                        "internal server error",
                        None,
                        &[],
                    );
                };
                let body =
                    serde_json::to_vec(&snapshot).expect("account snapshot is JSON serializable");
                return response("HTTP/1.1 200 OK", "application/json", &body, &[]);
            }
            if request.method == RequestMethod::Get && path == "/api/integration" {
                return read_integration_request(state);
            }
            if request.method == RequestMethod::Get
                && let Some(raw_account) = path
                    .strip_prefix("/api/accounts/")
                    .and_then(|rest| rest.strip_suffix("/quota-history"))
            {
                if raw_account.is_empty() {
                    return not_found_response();
                }
                let account_id = percent_decode(raw_account, false);
                let range = query_values(request.target, "range")
                    .into_iter()
                    .find(|value| !value.is_empty())
                    .unwrap_or_else(|| "1d".to_owned());
                // An explicit start/end (Unix seconds, like /api/usage) overrides `range`.
                let bound = |name| {
                    query_values(request.target, name)
                        .into_iter()
                        .find_map(|value| value.parse::<f64>().ok())
                        .filter(|value| value.is_finite())
                        .map(|value| value.trunc() as i64)
                };
                let period = bound("start").zip(bound("end"));
                return match quota_history_response(
                    state,
                    &account_id,
                    &range,
                    period,
                    now.trunc() as i64,
                ) {
                    Ok(payload) => {
                        let body = serde_json::to_vec(&payload)
                            .expect("quota history snapshot is serializable");
                        response("HTTP/1.1 200 OK", "application/json", &body, &[])
                    }
                    Err(QuotaHistoryResponseError::History(error)) => {
                        json_error_response(400, status_text(400), &error.to_string(), None, &[])
                    }
                    Err(QuotaHistoryResponseError::Account(error)) => {
                        json_error_response(404, status_text(404), &error.to_string(), None, &[])
                    }
                };
            }
            if request.method == RequestMethod::Delete
                && let Some(raw_account) = path.strip_prefix("/api/accounts/")
            {
                let raw_account = raw_account.trim_end_matches('/');
                if raw_account.is_empty() {
                    return not_found_response();
                }
                let account_id = percent_decode(raw_account, false);
                return match delete_account_state(request.observation_id, state, &account_id) {
                    Ok(()) => {
                        let body = serde_json::to_vec(&serde_json::json!({"status":"ok"})).unwrap();
                        response("HTTP/1.1 200 OK", "application/json", &body, &[])
                    }
                    Err(error) => json_error_response(400, status_text(400), &error, None, &[]),
                };
            }
            return not_found_response();
        }
        return unauthorized_response();
    }

    not_found_response()
}

/// Exchange the single-use bootstrap token for the management session.
fn bootstrap_session(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let Some(supplied) = (request.header_count(BOOTSTRAP_HEADER) == 1)
        .then(|| request.header(BOOTSTRAP_HEADER))
        .flatten()
        .filter(|value| !value.is_empty() && value.is_ascii())
    else {
        return unauthorized_response();
    };
    if !state.bootstrap.matches(supplied, now) || !state.bootstrap.consume() {
        return unauthorized_response();
    }
    let Some((token, expires_in)) = state.sessions.rotate(now) else {
        state.bootstrap.release();
        return json_error_response(500, status_text(500), "internal server error", None, &[]);
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "session": token,
        "expires_in": expires_in,
    }))
    .expect("session response is JSON serializable");
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &body,
        &[("Set-Cookie", CLEAR_LEGACY_SESSION_COOKIE)],
    )
}
