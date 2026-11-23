use crate::api::accounts::AccountRead;
use crate::api::catalog;
use crate::api::inspection;
use crate::api::provider_quota;
use crate::app::ServerState;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::RequestMethod;
use crate::http::request::percent_decode;
use crate::http::response::health_response;
use crate::http::response::json_error_response;
use crate::http::response::not_found_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::server::http::management::{ManagementSession, exchange, page_origin};
use crate::server::http::proxy::require_model_catalog;
use crate::server::http::settings::SettingsRead;
use crate::services::accounts::delete_account_state;
use crate::web::ui_response;

pub(crate) fn route_request_at(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let path = request.raw_path();
    let same_origin = same_origin(request, state.port);
    if request.method == RequestMethod::Get
        && (path.starts_with("/guide/") || path.starts_with("/assets/tutorial-"))
    {
        if let Err(error) = page_origin(request, state.port) {
            return error.wire_response();
        }
        return crate::services::guide_assets::asset_response(path)
            .unwrap_or_else(not_found_response);
    }
    if request.method == RequestMethod::Get
        && matches!(path, "/assets/app-icon.png" | "/assets/app-icon-source.png")
    {
        if let Err(error) = page_origin(request, state.port) {
            return error.wire_response();
        }
        let icon = state.backend.app_icon.snapshot();
        return response(
            "HTTP/1.1 200 OK",
            "image/png",
            if path.ends_with("-source.png") {
                &icon.source_png
            } else {
                &icon.png
            },
            &[("Cache-Control", "no-cache")],
        );
    }
    if request.method == RequestMethod::Get
        && let Some(bytes) = crate::web::asset_response(path).or_else(|| {
            same_origin
                .then(|| crate::web::module_asset_response(request, state))
                .flatten()
        })
    {
        return match page_origin(request, state.port) {
            Ok(()) => bytes,
            Err(error) => error.wire_response(),
        };
    }
    if request.method == RequestMethod::Get && catalog::models_path(path) {
        return match require_model_catalog(request, state, now)
            .and_then(|()| catalog::models_request(request, state))
        {
            Ok(reply) => reply.wire_response(),
            Err(error) => error.wire_response(),
        };
    }
    if path == "/healthz" {
        if let Err(error) = page_origin(request, state.port) {
            return error.wire_response();
        }
        return health_response();
    }
    if request.method == RequestMethod::Get && path == "/api/updates" {
        return crate::api::updates::read(request, state, now);
    }

    if path == "/" || path == "/index.html" {
        if let Err(error) = page_origin(request, state.port) {
            return error.wire_response();
        }
        return ui_response();
    }

    if request.method == RequestMethod::Post && path == "/api/session" {
        return match exchange(request, state, now) {
            Ok(reply) => reply.wire_response(),
            Err(error) => error.wire_response(),
        };
    }

    if path.starts_with("/api/") {
        if let Err(error) = ManagementSession::require(request, state, now) {
            return error.wire_response();
        }
        if request.method == RequestMethod::Get && path == "/api/team/status" {
            return crate::api::team::read(state);
        }
        if request.method == RequestMethod::Get && path == "/api/team/notices" {
            return crate::api::team::notices(state);
        }
        if request.method == RequestMethod::Get && path == "/api/image-credit" {
            return crate::api::images::credit_status(state);
        }
        if request.method == RequestMethod::Get && path == "/api/modules" {
            return crate::api::modules::read(state);
        }
        if request.method == RequestMethod::Get
            && matches!(path, "/api/gateway/clients" | "/api/gateway/usage")
        {
            return crate::api::gateway::read(request, state);
        }
        if request.method == RequestMethod::Get
            && matches!(path, "/api/gateway" | "/api/decisions/connection")
        {
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
        if request.method == RequestMethod::Get
            && let Some(read) = SettingsRead::for_path(path)
        {
            return read.read(state).wire_response();
        }
        if request.method == RequestMethod::Get
            && matches!(
                path,
                "/api/models/vision-test-image"
                    | "/api/models/audio-test-sound"
                    | "/api/capabilities"
            )
        {
            return inspection::read_request(request, state);
        }
        if request.method == RequestMethod::Get && catalog::provider_models_path(path) {
            return catalog::provider_models_request(path, state).wire_response();
        }
        if request.method == RequestMethod::Get
            && let Some(read) = AccountRead::for_path(path)
        {
            return match read.read(request, state, now) {
                Ok(reply) => reply.wire_response(),
                Err(error) => error.wire_response(),
            };
        }
        if request.method == RequestMethod::Get
            && let Some(id) = provider_quota::history_id(path)
        {
            return provider_quota::history(request, state, id, now).wire_response();
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

    not_found_response()
}
