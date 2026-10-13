//! HTTP delivery of a buffered, validated official agy result.
use crate::api::responses::ResponsesRequestResult;
use crate::api::streaming::{ObservedSse, serve_generated_response};
use crate::app::ServerState;
use crate::http::response::{response, status_text};
use crate::services::observation::request::RequestObservation;
use crate::services::{agy_cli, compaction};
use emp_core::ResolvedRoute;
use emp_router::ProjectionIds;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpStream;

pub(crate) fn http_error(error: agy_cli::Error) -> Vec<u8> {
    let event = error.event();
    let status = error.details().0;
    response(
        &format!("HTTP/1.1 {status} {}", status_text(status)),
        "application/json",
        &serde_json::to_vec(&json!({"error":event["error"]})).expect("agy error JSON"),
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn serve_http(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    stream: &mut TcpStream,
    observation: &mut RequestObservation,
    streaming: bool,
) -> ResponsesRequestResult {
    let mut monitor = crate::services::disconnect::DisconnectMonitor::start(stream).ok();
    let _activity =
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                route,
            ));
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        route,
        body,
        incoming,
        None,
        "responses",
    );
    let compact = compaction::has_trailing_compaction_trigger(body);
    let summary;
    let request = if compact {
        summary = compaction::compaction_summary_body(body);
        &summary
    } else {
        body
    };
    let completion = match agy_cli::execute(state, route, request, monitor.as_mut(), &mut usage) {
        Ok(result) => result,
        Err(agy_cli::Error("client_disconnected")) => return ResponsesRequestResult::Streamed,
        Err(error) => return ResponsesRequestResult::Buffered(http_error(error)),
    };
    usage.http_status(completion.status);
    usage.observe(&completion.body);
    usage.finish();
    let mut value = completion.body;
    if compact {
        let Some(summary) = compaction::response_output_text(&value) else {
            return ResponsesRequestResult::Buffered(http_error(agy_cli::Error(
                "agy_cli_invalid_output",
            )));
        };
        value = match compaction::external_compaction_from_summary(body, &summary) {
            Ok(value) => value,
            Err(error) => {
                return ResponsesRequestResult::Buffered(
                    crate::api::history_response::compaction_error_response(error),
                );
            }
        };
    } else {
        crate::services::context::record(state, route, body, true);
    }
    if streaming && !compact {
        return match serve_generated_response(ObservedSse::new(stream, observation), value, ids) {
            Ok(()) => ResponsesRequestResult::Streamed,
            Err(error) => ResponsesRequestResult::Buffered(error),
        };
    }
    ResponsesRequestResult::Buffered(response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(&value).expect("agy response JSON"),
        &[],
    ))
}
