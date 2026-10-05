//! Api compact.

use crate::api::failure_response::request_router_error_response;
use crate::api::failure_response::route_resolution_response;
use crate::api::history_response::destination_error_response;
use crate::api::history_response::history_http_error;
use crate::app::ServerState;
use crate::http::auth::proxy_allowed;
use crate::http::auth::same_origin;
use crate::http::request::Request;
use crate::http::request::read_json_body;
use crate::http::response::body_error_response;
use crate::http::response::json_error_response;
use crate::http::response::response;
use crate::http::response::status_text;
use crate::services::compaction::external_compaction_response;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::native;
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::providers::persist_protocol_observation;
use crate::services::request_preparation::{
    PreparedRequest, RequestOperation, RequestPreparationError, prepare_request,
};
use crate::util::projection_ids;
use std::collections::BTreeMap;
use std::net::TcpStream;

pub(crate) fn compact_request(
    stream: &mut TcpStream,
    request: Request<'_>,
    body_prefix: Vec<u8>,
    state: &ServerState,
    now: f64,
    observation: &mut RequestObservation,
) -> Vec<u8> {
    if !proxy_allowed(request, state, now) {
        let status = if same_origin(request, state.port) {
            401
        } else {
            403
        };
        return json_error_response(
            status,
            status_text(status),
            "proxy caller authentication is required",
            None,
            &[],
        );
    }
    observation.phase(Phase::ReadBody);
    let body = match read_json_body(stream, request, body_prefix, state) {
        Ok(body) => body,
        Err(error) => return body_error_response(error),
    };
    observation.body(body.get("reasoning"), body.get("stream"));
    observation.phase(Phase::ResolveRoute);
    let PreparedRequest {
        config,
        route,
        mut body,
    } = match prepare_request(state, body, RequestOperation::Compact) {
        Ok(prepared) => prepared,
        Err(RequestPreparationError::ModelRequired) => {
            return request_router_error_response(400, "request.model is required");
        }
        Err(RequestPreparationError::Route(error)) => return route_resolution_response(error),
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    observation.selected(&route);
    let incoming = request
        .headers
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
        .collect::<BTreeMap<_, _>>();
    let incoming = observation.headers(incoming);
    observation.phase(Phase::PrepareHistory);
    body = match prepare_history(state, &route, body, &incoming) {
        Ok(body) => body,
        Err(error) => return history_http_error(&error, observation),
    };
    observation.phase(Phase::PrepareDestination);
    let destination_context = {
        let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
        prepare_destination_context(state, &route, body, &incoming, monitor.as_mut())
    };
    body = match destination_context {
        Ok(body) => body,
        Err(error) => return destination_error_response(error, observation),
    };
    observation.phase(Phase::Execute);
    if route.dialect == emp_core::Dialect::CodexNative {
        return crate::api::native_response::complete_response(native::compact(
            state,
            &route,
            &config,
            body.as_object().expect("validated request object"),
            &incoming,
        ));
    }
    let ids = match projection_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return json_error_response(500, status_text(500), "internal server error", None, &[]);
        }
    };
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state, &route, &body, &incoming, None, "compact",
    );
    let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
    let (compacted, candidate) =
        match external_compaction_response(state, &route, &body, &incoming, &ids, monitor.as_mut())
        {
            Ok(result) => result,
            Err(error) => return crate::api::history_response::compaction_error_response(error),
        };
    usage.observe(&compacted);
    persist_protocol_observation(state, &candidate);
    match serde_json::to_vec(&compacted) {
        Ok(body) => response("HTTP/1.1 200 OK", "application/json", &body, &[]),
        Err(_) => json_error_response(500, status_text(500), "internal server error", None, &[]),
    }
}
