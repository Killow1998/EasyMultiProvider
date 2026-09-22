//! HTTP method/path dispatch. Domain logic lives in services.

use crate::api::accounts::management_account_import_request;
use crate::api::catalog;
use crate::api::compact::compact_request;
use crate::api::inspection;
use crate::api::integration::{management_integration_request, read_integration_request};
use crate::api::lifecycle::quit_request;
use crate::api::migration::management_migration_request;
use crate::api::quota::management_quota_request;
use crate::api::quota::serve_quota_events;
use crate::api::responses::ResponsesRequestResult;
use crate::api::responses::responses_request;
use crate::api::search::native_search_request;
use crate::api::websocket::serve_responses_websocket;
use crate::app::ServerState;
use crate::http::auth::bootstrap_value;
use crate::http::auth::same_origin;
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
use crate::services::quota::QuotaHistoryResponseError;
use crate::services::quota::quota_history_response;
use crate::util::system_now;
use crate::web::login_response;
use crate::web::redirect_response;
use crate::web::ui_response;
use std::io::Write;
use std::net::Shutdown;
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) fn handle_connection(mut stream: TcpStream, state: &ServerState) {
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let raw = match read_request_head(&mut stream) {
        Some(raw) => raw,
        None => {
            let _ = stream.write_all(&bad_request_response());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    let mut stop_after_write = false;
    let response = match parse_request(&raw.head) {
        Some(request)
            if request.method == RequestMethod::Post
                && matches!(
                    request.raw_path(),
                    "/api/runtime/scan" | "/api/runtime/select"
                ) =>
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
            serve_responses_websocket(&mut stream, request, state, system_now());
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
            Some(management_account_import_request(
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
                    "/api/migration/export" | "/api/migration/import"
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
                    "/api/providers/discover" | "/api/catalog/refresh" | "/api/config"
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
            ))
        }
        Some(request)
            if request.method == RequestMethod::Post && request.raw_path() == "/v1/responses" =>
        {
            match responses_request(&mut stream, request, raw.body_prefix, state, system_now()) {
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
    };
    if let Some(response) = response {
        let _ = stream.write_all(&response);
        let _ = stream.flush();
    }
    let _ = stream.shutdown(Shutdown::Write);
    if stop_after_write {
        state.shutdown.store(true, Ordering::Release);
    }
}

fn route_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    route_request_at(request, state, system_now())
}

pub(crate) fn route_request_at(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let path = request.raw_path();
    if request.method == RequestMethod::Get
        && (path == "/v1/models" || path.starts_with("/v1/models/"))
    {
        return catalog::models_request(request, state);
    }
    if path == "/healthz" {
        return health_response();
    }

    let same_origin = same_origin(request, state.port);
    if path == "/" || path == "/index.html" {
        if !same_origin {
            return cross_origin_response("cross-origin Web UI request rejected");
        }
        let supplied_cookie = request.session_cookie();
        if state.sessions.contains(supplied_cookie.as_deref(), now) {
            let Some(cookie) = state.sessions.refresh_header(now) else {
                return login_response();
            };
            return ui_response(&cookie);
        }
        let Some(supplied) = bootstrap_value(request.target) else {
            return login_response();
        };
        if !state.bootstrap.matches(&supplied) {
            return login_response();
        }
        let Some(cookie) = state.sessions.refresh_or_rotate_header(now) else {
            return login_response();
        };
        if !state.bootstrap.consume() {
            return login_response();
        }
        return redirect_response(&cookie);
    }

    if path.starts_with("/api/") {
        if !same_origin {
            return cross_origin_response("management session is required");
        }
        let supplied_cookie = request.session_cookie();
        if state.sessions.contains(supplied_cookie.as_deref(), now) {
            if request.method == RequestMethod::Get
                && matches!(
                    path,
                    "/api/models/vision-test-image" | "/api/request-limits"
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
                && path.starts_with("/api/accounts/")
                && path.ends_with("/quota-history")
            {
                let raw_account =
                    &path["/api/accounts/".len()..path.len() - "/quota-history".len()];
                let account_id = percent_decode(raw_account, false);
                let range = query_values(request.target, "range")
                    .into_iter()
                    .find(|value| !value.is_empty())
                    .unwrap_or_else(|| "1d".to_owned());
                return match quota_history_response(state, &account_id, &range, now.trunc() as i64)
                {
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
            if request.method == RequestMethod::Delete && path.starts_with("/api/accounts/") {
                let account_id =
                    percent_decode(path["/api/accounts/".len()..].trim_end_matches('/'), false);
                return match delete_account_state(state, &account_id) {
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
