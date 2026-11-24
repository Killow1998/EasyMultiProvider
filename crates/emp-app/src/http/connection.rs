use crate::api::catalog;
use crate::api::integration::management_integration_request;
use crate::api::lifecycle::quit_request;
use crate::api::quota::serve_quota_events;
use crate::api::realtime::serve_realtime_call;
use crate::api::realtime::sideband::serve_realtime_sideband;
use crate::api::websocket::serve_responses_websocket;
use crate::app::ServerState;
use crate::http::request::REQUEST_READ_TIMEOUT;
use crate::http::request::RequestMethod;
use crate::http::request::parse_request;
use crate::http::request::percent_decode;
use crate::http::request::{read_json_body, read_request_head};
use crate::http::response::bad_request_response;
use crate::http::routes::route_request_at;
use crate::server::http::accounts::AccountWriteRoute;
use crate::server::http::catalog::CatalogWriteRoute;
use crate::server::http::inference::InferenceRoute;
use crate::server::http::local_writes::LocalWriteRoute;
use crate::server::http::proxy::{ConnectionIntake, RequestAccess, request_source};
use crate::server::http::usage::Report;
use crate::server::http::{
    error::EntryError,
    gateway::{self, GatewayClientWrite},
    login,
    management::ManagementSession,
    migration::MigrationRoute,
    modules::{self, ModuleAction},
    team,
};
use crate::services::modules::aero::CallLease;
use crate::services::observation::http::HttpObservation;
use crate::util::system_now;
use std::net::Shutdown;
use std::net::TcpStream;

pub(crate) fn handle_connection(
    mut stream: TcpStream,
    state: &std::sync::Arc<ServerState>,
    intake: ConnectionIntake,
    observation: HttpObservation,
) {
    let request_permit;
    let mut observation = observation;
    if stream.set_nonblocking(false).is_err()
        || stream.set_nodelay(true).is_err()
        || stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).is_err()
    {
        observation.failure("socket_setup_failed");
        return;
    }
    let (head, body_prefix) = read_request_head(&mut stream)
        .map(|raw| (raw.head, raw.body_prefix))
        .unwrap_or_default();
    let request = parse_request(&head);
    let peer = stream.peer_addr().ok().map(|peer| peer.ip());
    let Some((permit, callback)) = intake.admit(request, state, peer) else {
        crate::lifecycle::connections::request_rejected(state, "connection_capacity");
        return;
    };
    let Some(mut request) = request else {
        observation.write_response(&mut stream, &bad_request_response());
        let _ = stream.shutdown(Shutdown::Write);
        return;
    };
    let mut stop_after_write = false;
    let observation_id = observation.request_id().map(str::to_owned);
    request.observation_id = observation_id.as_deref();
    let (delegation, callback_work) = callback
        .map(|callback| (Some(callback.identity), callback.work))
        .unwrap_or_default();
    let mut source = request_source(request, state, system_now(), peer, delegation);
    observation.received(request);
    let path = request.raw_path();
    request_permit = (!request.releases_request_permit()).then_some(permit);
    crate::services::runtime::codex_contacted(request, state);
    let access = match RequestAccess::require(request, state, callback_work.as_ref()) {
        Ok(access) => access,
        Err(error) => {
            observation.write_response(&mut stream, &error.wire_response());
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    source.aero_lease =
        request_permit
            .as_ref()
            .zip(access.permit.as_ref())
            .map(|(connection, work)| CallLease {
                connection: connection.clone(),
                work: work.clone(),
            });
    let request = crate::http::request::Request {
        source: Some(&source),
        ..request
    };
    let now = system_now();
    let response = if let Some(route) = AccountWriteRoute::for_request(request) {
        Some(route.legacy(&mut stream, request, body_prefix, state, now))
    } else if let Some(route) = CatalogWriteRoute::for_request(request) {
        Some(
            route
                .legacy(&mut stream, request, body_prefix, state, now)
                .unwrap_or_else(EntryError::reply)
                .wire_response(),
        )
    } else if let Some(route) = LocalWriteRoute::for_request(request) {
        Some(
            route
                .legacy(&mut stream, request, body_prefix, state, now)
                .unwrap_or_else(EntryError::reply)
                .wire_response(),
        )
    } else if let Some(route) = InferenceRoute::for_request(request) {
        route.legacy(
            &mut stream,
            request,
            body_prefix,
            state,
            now,
            observation.model.as_mut(),
        )
    } else if let Some(route) = MigrationRoute::for_request(request) {
        Some(
            route
                .legacy(&mut stream, request, body_prefix, state, now)
                .unwrap_or_else(|reply| reply)
                .wire_response(),
        )
    } else if login::matches(request) {
        Some(
            login::legacy(&mut stream, request, body_prefix, state, now)
                .unwrap_or_else(EntryError::reply)
                .wire_response(),
        )
    } else {
        match (request.method, path) {
            _ if team::matches_write(request) => Some(
                team::legacy(&mut stream, request, body_prefix, state, now)
                    .unwrap_or_else(EntryError::reply)
                    .wire_response(),
            ),
            (RequestMethod::Post, modules::PATH)
            | (RequestMethod::Post | RequestMethod::Delete, gateway::CLIENTS_PATH) => {
                let reply = ManagementSession::require(request, state, now).and_then(|_| {
                    let body = read_json_body(&mut stream, request, body_prefix, state)?;
                    if path == modules::PATH {
                        Ok(ModuleAction::parse(body)?.execute(state))
                    } else {
                        let remove = request.method == RequestMethod::Delete;
                        Ok(GatewayClientWrite::parse(body, remove)?.execute(state))
                    }
                });
                Some(reply.unwrap_or_else(EntryError::reply).wire_response())
            }
            (RequestMethod::Post, "/api/updates/check" | "/api/updates/install") => Some(
                crate::api::updates::start(&mut stream, request, body_prefix, state, now),
            ),
            (RequestMethod::Post, "/api/quit") => {
                let (response, stop) = quit_request(&mut stream, request, body_prefix, state, now);
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
                    body_prefix,
                    state,
                    now,
                    observation.request_id(),
                );
                None
            }
            (RequestMethod::Post, "/v1/live") => Some(serve_realtime_call(
                &mut stream,
                request,
                body_prefix,
                state,
                now,
            )),
            (RequestMethod::Get, path) if path.starts_with("/v1/live/") => {
                let call_id = percent_decode(&path["/v1/live/".len()..], false);
                serve_realtime_sideband(&mut stream, request, &call_id, body_prefix, state, now);
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
                body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post, "/api/models/metadata") => Some(catalog::management_request(
                &mut stream,
                request,
                body_prefix,
                state,
                now,
            )),
            (RequestMethod::Get, "/api/calls") => {
                Report::Calls.serve_legacy(&mut stream, request, state, now)
            }
            (RequestMethod::Get, "/api/usage") => {
                Report::Usage.serve_legacy(&mut stream, request, state, now)
            }
            (RequestMethod::Get, "/api/accounts/events") => {
                serve_quota_events(&mut stream, request, state, now);
                None
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
