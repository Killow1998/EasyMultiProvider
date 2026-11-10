//! Single-step Claude completion projected into downstream WebSocket events.
use super::{Turn, TurnResult};
use crate::app::ServerState;
use serde_json::Value;

pub(super) fn serve(
    state: &ServerState,
    turn: &Turn,
    websocket: &mut super::ObservedWebSocket<'_, '_>,
    monitor: &mut crate::services::disconnect::DisconnectMonitor,
) -> TurnResult {
    let Turn {
        route,
        body: request_body,
        headers: request_headers,
        ids,
        ..
    } = turn;
    // Expose an ID before CLI startup, so Codex can steer while waiting.
    let id = ids.response_id().to_owned();
    if websocket
        .send_json(&serde_json::json!({"type":"response.created", "response":{
            "id":id, "object":"response", "status":"in_progress", "output":[]
        }}))
        .is_err()
    {
        return TurnResult::Closed;
    }
    let _activity_guard =
        state
            .backend
            .activity
            .begin(crate::services::activity::ActivityIdentity::from_route(
                route,
            ));
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        route,
        request_body,
        request_headers,
        None,
        "responses",
    )
    .started_at(websocket.observation.execution_started())
    .transport("websocket");
    let mut live = crate::api::claude_stream::LiveText::new(ids, true, |event: &Value| {
        websocket
            .send_stateless_json(&mut event.clone(), false)
            .map_err(std::io::Error::other)
    });
    let completion = match crate::services::claude_cli::execute_complete(
        state,
        route,
        request_body,
        request_headers,
        Some(monitor),
        &mut usage,
        &mut |id, text| live.push(id, text),
    ) {
        Ok(completion) => completion,
        Err(
            crate::services::claude_cli::ClaudeCliError::Disconnected
            | crate::services::claude_cli::ClaudeCliError::Failure("claude_cli_interrupted"),
        ) => {
            drop(live);
            if websocket.interrupted() {
                usage.interrupted();
                return TurnResult::Finished;
            }
            usage.disconnected();
            return TurnResult::Closed;
        }
        Err(error) => {
            drop(live);
            if websocket
                .send_json(&crate::api::claude_response::websocket_value_for_route(
                    &error, route,
                ))
                .is_err()
            {
                return TurnResult::Closed;
            }
            return TurnResult::Finished;
        }
    };
    let selected_route = completion.route;
    let mut response_value = completion.response.body;
    response_value["id"] = Value::String(id);
    usage.candidate(&selected_route);
    usage.http_status(completion.response.status);
    usage.upstream_observation(&completion.response.observation);
    usage.observe(&response_value);
    usage.finish();
    if response_value["status"] == "completed" {
        crate::services::context::record(state, &selected_route, request_body, true);
    }
    crate::services::providers::persist_protocol_observation(state, &selected_route);
    if live.finish(response_value).is_err() {
        return TurnResult::Closed;
    }
    TurnResult::Finished
}
