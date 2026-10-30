//! Buffered HTTP responses. Domain logic lives in services.

use crate::api::catalog;
use crate::api::inspection;
use crate::api::integration::read_integration_request;
use crate::app::ServerState;
use crate::http::auth::BOOTSTRAP_HEADER;
use crate::http::auth::CLEAR_LEGACY_SESSION_COOKIE;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::RequestMethod;
use crate::http::request::percent_decode;
use crate::http::request::query_values;
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
use crate::web::ui_response;

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
        if !same_origin && !crate::http::auth::proxy_allowed(request, state, now) {
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
            if request.method == RequestMethod::Get && path == "/api/gateway" {
                return response(
                    "HTTP/1.1 200 OK",
                    "application/json",
                    &serde_json::to_vec(&serde_json::json!({"api_key":state.backend.gateway_key}))
                        .expect("gateway JSON"),
                    &[("Cache-Control", "no-store")],
                );
            }
            if request.method == RequestMethod::Get && path == "/api/support-report" {
                return crate::api::support_report::read(state);
            }
            if request.method == RequestMethod::Get && path == "/api/diagnostics" {
                return crate::api::diagnostics::read(state);
            }
            if request.method == RequestMethod::Get && path == "/api/calls" {
                return crate::api::calls::read(request, state);
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
                    || ((path.starts_with("/api/accounts/")
                        || path.starts_with("/api/providers/"))
                        && path.ends_with("/models")))
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
            if request.method == RequestMethod::Get
                && let Some(id) = path
                    .strip_prefix("/api/providers/")
                    .and_then(|path| path.strip_suffix("/quota-history"))
            {
                return crate::api::provider_quota::history(
                    request,
                    state,
                    &percent_decode(id, false),
                    now,
                );
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
    // A tray click issues one fresh link, without persisting another credential
    // or turning the original ten-minute startup link into a reusable secret.
    let reopened = state.reopen_bootstrap.lock().ok();
    let bootstrap = if state.bootstrap.matches(supplied, now) {
        &state.bootstrap
    } else if let Some(token) = reopened
        .as_ref()
        .and_then(|guard| guard.as_ref())
        .filter(|token| token.matches(supplied, now))
    {
        token
    } else {
        return unauthorized_response();
    };
    if !bootstrap.consume() {
        return unauthorized_response();
    }
    let Some((token, expires_in)) = state.sessions.rotate(now) else {
        bootstrap.release();
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
