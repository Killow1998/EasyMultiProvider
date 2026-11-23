use crate::api::catalog;
use crate::api::compact::compact_request;
use crate::api::integration::management_integration_request;
use crate::api::lifecycle::quit_request;
use crate::api::migration::management_migration_request;
use crate::api::quota::serve_quota_events;
use crate::api::realtime::serve_realtime_call;
use crate::api::realtime::sideband::serve_realtime_sideband;
use crate::api::responses::ResponsesRequestResult;
use crate::api::responses::responses_request;
use crate::api::search::native_search_request;
use crate::api::websocket::serve_responses_websocket;
use crate::app::ServerState;
use crate::http::request::REQUEST_READ_TIMEOUT;
use crate::http::request::RequestHead;
use crate::http::request::RequestMethod;
use crate::http::request::parse_request;
use crate::http::request::percent_decode;
use crate::http::request::read_request_head;
use crate::http::response::bad_request_response;
use crate::http::routes::route_request_at;
use crate::server::http::accounts::AccountWriteRoute;
use crate::server::http::catalog::CatalogWriteRoute;
use crate::server::http::proxy::{RequestAccess, request_source};
use crate::server::http::usage::Report;
use crate::server::http::{error::EntryError, login};
use crate::services::connection_admission::ConnectionPermit;
use crate::services::observation::http::HttpObservation;
use crate::util::system_now;
use std::net::Shutdown;
use std::net::TcpStream;

pub(crate) fn handle_connection(
    mut stream: TcpStream,
    state: &ServerState,
    request_permit: ConnectionPermit,
    head: Option<RequestHead>,
    observation: HttpObservation,
) {
    let mut request_permit = Some(request_permit);
    let mut observation = observation;
    if stream.set_nonblocking(false).is_err()
        || stream.set_nodelay(true).is_err()
        || stream.set_read_timeout(Some(REQUEST_READ_TIMEOUT)).is_err()
    {
        observation.failure("socket_setup_failed");
        return;
    }
    let raw = match head.or_else(|| read_request_head(&mut stream)) {
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
    let peer = stream.peer_addr().ok().map(|peer| peer.ip());
    let source = request_source(request, state, system_now(), peer);
    request.source = Some(&source);
    observation.received(request);
    let path = request.raw_path();
    if request.releases_request_permit() {
        let _ = request_permit.take();
    }
    crate::services::runtime::codex_contacted(request, state);
    let _access = match RequestAccess::require(request, state) {
        Ok(access) => access,
        Err(error) => {
            observation.write_response(&mut stream, &error.wire_response());
            let _ = stream.shutdown(Shutdown::Write);
            return;
        }
    };
    let now = system_now();
    let response = if let Some(route) = AccountWriteRoute::for_request(request) {
        Some(route.legacy(&mut stream, request, raw.body_prefix, state, now))
    } else if let Some(route) = CatalogWriteRoute::for_request(request) {
        Some(
            route
                .legacy(&mut stream, request, raw.body_prefix, state, now)
                .unwrap_or_else(EntryError::reply)
                .wire_response(),
        )
    } else if login::matches(request) {
        Some(
            login::legacy(&mut stream, request, raw.body_prefix, state, now)
                .unwrap_or_else(EntryError::reply)
                .wire_response(),
        )
    } else {
        match (request.method, path) {
            (RequestMethod::Post, path) if path.starts_with("/api/team/") => Some(
                crate::api::team::mutate(&mut stream, request, raw.body_prefix, state, now),
            ),
            (RequestMethod::Post, "/api/modules") => Some(crate::api::modules::mutate(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
            (RequestMethod::Post | RequestMethod::Delete, "/api/gateway/clients") => Some(
                crate::api::gateway::mutate(&mut stream, request, raw.body_prefix, state, now),
            ),
            (RequestMethod::Post, "/v1/decisions" | "/v1/systemone") => Some(
                crate::api::decisions::request(&mut stream, request, raw.body_prefix, state, now),
            ),
            (RequestMethod::Post, "/api/updates/check" | "/api/updates/install") => Some(
                crate::api::updates::start(&mut stream, request, raw.body_prefix, state, now),
            ),
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
            (RequestMethod::Post, "/api/models/metadata" | "/api/language") => Some(
                catalog::management_request(&mut stream, request, raw.body_prefix, state, now),
            ),
            (RequestMethod::Post, "/v1/responses/compact") => match compact_request(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
                observation.model.as_mut().expect("model HTTP observation"),
            ) {
                ResponsesRequestResult::Buffered(response) => Some(response),
                ResponsesRequestResult::Streamed => None,
            },
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
            (RequestMethod::Post, "/api/app-icon") => Some(crate::api::app_icon::save(
                &mut stream,
                request,
                raw.body_prefix,
                state,
                now,
            )),
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
