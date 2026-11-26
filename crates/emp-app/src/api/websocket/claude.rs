use super::{ObservedWebSocket, Turn, TurnResult, control::TurnControl};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::providers::save_learned_protocol;
use emp_state::{config::LearnedProtocol, observed_at_now};
use serde_json::Value;
use std::{io, sync::Arc};
use tokio::sync::{mpsc, oneshot};

enum Delivery {
    Event(Value, oneshot::Sender<io::Result<()>>),
    Finished(TurnResult, oneshot::Sender<()>),
}

pub(super) async fn serve(
    state: &Arc<ServerState>,
    turn: Turn,
    websocket: &mut ObservedWebSocket<'_>,
    mut monitor: DisconnectMonitor,
    control: TurnControl,
    pending_request: &mut Option<String>,
) -> TurnResult {
    let id = turn.ids.response_id().to_owned();
    if websocket
        .send_json(&serde_json::json!({"type":"response.created", "response":{
            "id":id, "object":"response", "status":"in_progress", "output":[]
        }}))
        .await
        .is_err()
    {
        return control
            .finish(websocket, pending_request, TurnResult::Closed)
            .await;
    }
    let state = Arc::clone(state);
    let started = websocket.observation.execution_started();
    let (events, mut incoming) = mpsc::channel(1);
    let worker = crate::server::run_blocking(move || {
        let Turn {
            route,
            body,
            headers,
            ids,
            ..
        } = turn;
        let mut usage = crate::services::request_outcome::RequestGuard::new(
            &state,
            &route,
            &body,
            &headers,
            None,
            "responses",
        )
        .started_at(started)
        .transport("websocket");
        let mut live = crate::api::claude_stream::LiveText::new(&ids, true);
        let send = |event: &Value| {
            let (acknowledge, receipt) = oneshot::channel();
            events
                .blocking_send(Delivery::Event(event.clone(), acknowledge))
                .map_err(io::Error::other)?;
            receipt.blocking_recv().map_err(io::Error::other)?
        };
        let completion = crate::services::claude_cli::execute_complete(
            &state,
            &route,
            &body,
            &headers,
            Some(&mut monitor),
            &mut usage,
            |_, id, text| live.push(id, text, send),
        );
        let result = match completion {
            Ok(completion) => {
                let learned = LearnedProtocol::from_route(
                    &route,
                    completion.route.protocol,
                    observed_at_now(),
                );
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
                live.finish(response, send)
            }
            Err(
                crate::services::claude_cli::ClaudeCliError::Disconnected
                | crate::services::claude_cli::ClaudeCliError::Failure("claude_cli_interrupted"),
            ) => {
                if monitor.is_interrupted() {
                    usage.interrupted();
                    Ok(())
                } else {
                    usage.disconnected();
                    Err(io::ErrorKind::BrokenPipe.into())
                }
            }
            Err(error) => {
                usage.finish();
                send(&crate::api::claude_response::websocket_value_for_route(
                    &error, &route,
                ))
            }
        };
        usage.finish();
        let result = if result.is_ok() {
            TurnResult::Finished
        } else {
            TurnResult::Closed
        };
        let (acknowledge, receipt) = oneshot::channel();
        if events
            .blocking_send(Delivery::Finished(result, acknowledge))
            .is_ok()
        {
            let _ = receipt.blocking_recv();
        }
    });
    let deliver = async {
        while let Some(delivery) = incoming.recv().await {
            match delivery {
                Delivery::Event(mut event, acknowledge) => {
                    let result = websocket
                        .send_stateless_json(&mut event, false)
                        .await
                        .map_err(io::Error::other);
                    let _ = acknowledge.send(result);
                }
                Delivery::Finished(result, acknowledge) => {
                    let result = control.finish(websocket, pending_request, result).await;
                    let _ = acknowledge.send(());
                    return result;
                }
            }
        }
        control
            .finish(websocket, pending_request, TurnResult::Closed)
            .await
    };
    let (_, result) = tokio::join!(worker, deliver);
    result
}
