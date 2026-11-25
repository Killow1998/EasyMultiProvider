use super::{ObservedWebSocket, Turn, TurnResult};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::providers::save_learned_protocol;
use emp_state::{config::LearnedProtocol, observed_at_now};
use serde_json::Value;
use std::{io, sync::Arc};
use tokio::sync::{mpsc, oneshot};

enum Completion {
    Finished(TurnResult),
    Failed(Value),
}

pub(super) async fn serve(
    state: &Arc<ServerState>,
    turn: Turn,
    websocket: &mut ObservedWebSocket<'_>,
    mut monitor: DisconnectMonitor,
) -> TurnResult {
    let id = turn.ids.response_id().to_owned();
    if websocket
        .send_json(&serde_json::json!({"type":"response.created", "response":{
            "id":id, "object":"response", "status":"in_progress", "output":[]
        }}))
        .await
        .is_err()
    {
        return TurnResult::Closed;
    }
    let state = Arc::clone(state);
    let started = websocket.observation.execution_started();
    let (events, mut incoming) = mpsc::channel::<(Value, oneshot::Sender<io::Result<()>>)>(1);
    let worker = crate::server::run_blocking(move || {
        let Turn {
            route,
            body,
            headers,
            ids,
            ..
        } = turn;
        let _activity =
            state
                .backend
                .activity
                .begin(crate::services::activity::ActivityIdentity::from_route(
                    &route,
                ));
        let mut usage = crate::services::request_outcome::RequestOutcome::new(
            &state,
            &route,
            &body,
            &headers,
            None,
            "responses",
        )
        .started_at(started)
        .transport("websocket");
        let mut live = crate::api::claude_stream::LiveText::new(&ids, true, |event: &Value| {
            let (acknowledge, receipt) = oneshot::channel();
            events
                .blocking_send((event.clone(), acknowledge))
                .map_err(io::Error::other)?;
            receipt.blocking_recv().map_err(io::Error::other)?
        });
        let completion = match crate::services::claude_cli::execute_complete(
            &state,
            &route,
            &body,
            &headers,
            Some(&mut monitor),
            &mut usage,
            &mut |id, text| live.push(id, text),
        ) {
            Ok(completion) => completion,
            Err(
                crate::services::claude_cli::ClaudeCliError::Disconnected
                | crate::services::claude_cli::ClaudeCliError::Failure("claude_cli_interrupted"),
            ) => {
                let result = if monitor.is_interrupted() {
                    usage.interrupted();
                    TurnResult::Finished
                } else {
                    usage.disconnected();
                    TurnResult::Closed
                };
                return Completion::Finished(result);
            }
            Err(error) => {
                return Completion::Failed(crate::api::claude_response::websocket_value_for_route(
                    &error, &route,
                ));
            }
        };
        let learned =
            LearnedProtocol::from_route(&route, completion.route.protocol, observed_at_now());
        let selected_route = completion.route;
        let mut response = completion.response.body;
        response["id"] = Value::String(id);
        usage.candidate(&selected_route);
        usage.http_status(completion.response.status);
        usage.upstream_observation(&completion.response.observation);
        usage.observe(&response);
        usage.finish();
        if response["status"] == "completed" {
            crate::services::context::record(&state, &selected_route, &body, true);
        }
        save_learned_protocol(&state, learned);
        Completion::Finished(if live.finish(response).is_ok() {
            TurnResult::Finished
        } else {
            TurnResult::Closed
        })
    });
    let deliver = async {
        while let Some((mut event, acknowledge)) = incoming.recv().await {
            let result = websocket
                .send_stateless_json(&mut event, false)
                .await
                .map_err(io::Error::other);
            let _ = acknowledge.send(result);
        }
    };
    let (result, ()) = tokio::join!(worker, deliver);
    match result {
        Ok(Completion::Finished(result)) => result,
        Ok(Completion::Failed(event)) if websocket.send_json(&event).await.is_ok() => {
            TurnResult::Finished
        }
        _ => TurnResult::Closed,
    }
}
