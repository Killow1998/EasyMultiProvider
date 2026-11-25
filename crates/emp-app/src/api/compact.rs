use crate::api::history_response::{PreparationOutput, prepare_http_error};
use crate::app::ServerState;
use crate::http::request::Request;
use crate::proxy::workflow::{self, ContextPreparation};
use crate::server::http::stream::BodyOutput;
use crate::server::http::{error::HttpReply, inference::InferenceOutcome};
use crate::services::compaction::external_compaction_response;
use crate::services::native;
use crate::services::observation::request::{Phase, RequestObservation};
use crate::services::providers::save_learned_protocol;
use crate::services::request_preparation::{PreparedRequest, RequestOperation};
use crate::util::projection_ids;
use emp_state::{config::LearnedProtocol, observed_at_now};

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
        body,
    } = match workflow::resolve(
        state,
        body,
        RequestOperation::Compact,
        &incoming,
        observation,
    ) {
        Ok(prepared) => prepared,
        Err(error) => {
            return prepare_http_error(state, error, observation, PreparationOutput::Compact);
        }
    };
    let body = match ContextPreparation::begin(state, &route, &incoming, observation).prepare(
        body,
        observation,
        || downstream.disconnect().ok(),
    ) {
        Ok(body) => body,
        Err(error) => {
            return prepare_http_error(state, error, observation, PreparationOutput::Compact);
        }
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
    let learned = LearnedProtocol::from_route(&route, candidate.protocol, observed_at_now());
    usage.observe(&compacted);
    save_learned_protocol(state, learned);
    match serde_json::to_vec(&compacted) {
        Ok(body) => HttpReply::new(200, body),
        Err(_) => HttpReply::error(500, "internal server error", None),
    }
    .into()
}
