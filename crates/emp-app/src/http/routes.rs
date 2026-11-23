use crate::api::accounts::AccountRead;
use crate::api::catalog;
use crate::api::gateway::GatewayRead;
use crate::api::provider_quota;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::request::RequestMethod;
use crate::http::response::not_found_response;
use crate::server::http::management::{ManagementSession, exchange};
use crate::server::http::modules::ModulePanel;
use crate::server::http::proxy::require_model_catalog;
use crate::server::http::settings::SettingsRead;
use crate::server::http::{inspection, pages, team};

pub(crate) fn route_request_at(request: Request<'_>, state: &ServerState, now: f64) -> Vec<u8> {
    let path = request.raw_path();
    if let Some(reply) = pages::legacy_reply(request, state) {
        return reply.wire_response();
    }
    if request.method == RequestMethod::Get
        && let Some(panel) = ModulePanel::for_path(path)
    {
        return panel
            .require_origin(request, state.port)
            .map(|panel| panel.read(request, state))
            .unwrap_or_else(|error| error.reply())
            .wire_response();
    }
    if request.method == RequestMethod::Get && catalog::models_path(path) {
        return match require_model_catalog(request, state, now)
            .and_then(|()| catalog::models_request(request, state))
        {
            Ok(reply) => reply.wire_response(),
            Err(error) => error.wire_response(),
        };
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
        if request.method == RequestMethod::Get && path == "/api/updates" {
            return crate::api::updates::read(state).wire_response();
        }
        if request.method == RequestMethod::Get
            && let Some(read) = team::read_handler(path).or_else(|| inspection::read_handler(path))
        {
            return read(state).wire_response();
        }
        if request.method == RequestMethod::Get && path == "/api/modules" {
            return crate::api::modules::read(state).wire_response();
        }
        if request.method == RequestMethod::Get
            && let Some(read) = GatewayRead::for_path(path)
        {
            return read.read(request, state).wire_response();
        }
        if request.method == RequestMethod::Get
            && let Some(read) = SettingsRead::for_path(path)
        {
            return read.read(state).wire_response();
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
        return not_found_response();
    }

    not_found_response()
}
