use crate::api::failure_response::request_router_error_response;
use crate::api::failure_response::route_resolution_response;
use crate::api::history_response::destination_error_response;
use crate::api::history_response::history_http_error;
use crate::app::ServerState;
use crate::http::request::Request;
use crate::server::http::stream::BodyOutput;
use crate::server::http::{error::HttpReply, inference::InferenceOutcome};
use crate::services::compaction::external_compaction_response;
use crate::services::history::prepare_destination_context;
use crate::services::history::prepare_history;
use crate::services::native;
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::providers::persist_protocol_observation;
use crate::services::request_preparation::{
    PreparedRequest, RequestOperation, RequestPreparationError,
};
use crate::util::projection_ids;

pub(crate) fn execute(
    downstream: &mut BodyOutput,
    request: Request<'_>,
    body: serde_json::Value,
    state: &ServerState,
    observation: &mut RequestObservation,
) -> InferenceOutcome {
    observation.body(
        body.get("reasoning"),
        body.get("input"),
        body.get("client_metadata"),
        body.get("stream"),
    );
    observation.phase(Phase::ResolveRoute);
    let incoming = observation.headers(request.incoming());
    if let Some(config) = crate::api::aero::selected_config(state, &body) {
        return crate::api::aero::relay(
            downstream,
            state,
            &config,
            &body,
            (request, &incoming),
            observation,
            true,
        );
    }
    let PreparedRequest {
        config,
        route,
        mut body,
    } = match crate::services::request_preparation::prepare_request(
        state,
        body,
        RequestOperation::Compact,
        &incoming,
    ) {
        Ok(prepared) => prepared,
        Err(RequestPreparationError::ModelRequired) => {
            return InferenceOutcome::Buffered(request_router_error_response(
                400,
                "request.model is required",
            ));
        }
        Err(RequestPreparationError::Route(error)) => {
            return InferenceOutcome::Buffered(route_resolution_response(error));
        }
        Err(RequestPreparationError::TeamAccess(error)) => {
            return InferenceOutcome::Buffered(super::native_response::error_reply(
                error.public_error(state),
            ));
        }
        Err(RequestPreparationError::ConfigurationUnavailable) => {
            return InferenceOutcome::Buffered(HttpReply::error(
                500,
                "configuration is unavailable",
                None,
            ));
        }
        Err(_) => {
            return InferenceOutcome::Buffered(HttpReply::error(
                500,
                "internal server error",
                None,
            ));
        }
    };
    observation.selected(&route);
    observation.phase(Phase::PrepareHistory);
    body = match prepare_history(state, &route, body, &incoming) {
        Ok(body) => body,
        Err(error) => return InferenceOutcome::Buffered(history_http_error(&error, observation)),
    };
    observation.phase(Phase::PrepareDestination);
    let destination_context = {
        let mut monitor = downstream.disconnect().ok();
        prepare_destination_context(state, &route, body, &incoming, monitor.as_mut())
    };
    body = match destination_context {
        Ok(body) => body,
        Err(error) => return destination_error_response(error, observation).into(),
    };
    observation.phase(Phase::Execute);
    if route.dialect == emp_domain::Dialect::CodexNative {
        return InferenceOutcome::Buffered(crate::api::native_response::complete_response(
            native::compact(state, &route, &config, &body, &incoming),
        ));
    }
    let ids = match projection_ids() {
        Ok(ids) => ids,
        Err(_) => {
            return InferenceOutcome::Buffered(HttpReply::error(
                500,
                "internal server error",
                None,
            ));
        }
    };
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state, &route, &body, &incoming, None, "compact",
    );
    let mut monitor = downstream.disconnect().ok();
    let (compacted, candidate) = match external_compaction_response(
        state,
        &route,
        &body,
        &incoming,
        &ids,
        monitor.as_mut(),
        &mut usage,
    ) {
        Ok(result) => result,
        Err(error) => return crate::api::history_response::compaction_error_response(error).into(),
    };
    usage.observe(&compacted);
    persist_protocol_observation(state, &candidate);
    match serde_json::to_vec(&compacted) {
        Ok(body) => HttpReply::new(200, body),
        Err(_) => HttpReply::error(500, "internal server error", None),
    }
    .into()
}
