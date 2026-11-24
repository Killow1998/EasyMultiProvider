//! Framed control ownership while a turn uses HTTP or the single-step CLI.
//! One bounded next request is retained; cancellation reuses the I/O race.
use super::socket::{Downstream, DownstreamEvent};
use super::{ObservedWebSocket, TurnResult};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Mutex, oneshot};

#[derive(Default)]
struct Response {
    id: Option<String>,
    terminal: bool,
    interruption_written: bool,
}

#[derive(Default)]
pub(super) struct Control {
    response: Mutex<Response>,
    pub(super) interrupted: Arc<AtomicBool>,
    previous_id: Option<String>,
}

impl Control {
    pub(super) async fn send(
        &self,
        websocket: &Downstream,
        event: &Value,
    ) -> Result<bool, emp_transport::WebSocketError> {
        let mut response = self.response.lock().await;
        let interruption = event["type"] == "response.incomplete"
            && event["response"]["incomplete_details"]["reason"] == "interrupted";
        if self.interrupted.load(Ordering::Acquire) && !interruption {
            return Ok(false);
        }
        websocket.writer.send_json(event).await?;
        if event["type"] == "response.created" {
            response.id = event["response"]["id"].as_str().map(str::to_owned);
        }
        response.terminal |= crate::services::events::terminal_stream_event(event);
        response.interruption_written |= interruption;
        Ok(true)
    }

    async fn incomplete(&self) -> Option<Value> {
        let response = self.response.lock().await;
        if !self.interrupted.load(Ordering::Acquire) || response.interruption_written {
            return None;
        }
        Some(json!({"type":"response.incomplete", "response":{
            "id":response.id.as_deref()?, "object":"response", "status":"incomplete",
            "incomplete_details":{"reason":"interrupted"}, "output":[], "end_turn":false
        }}))
    }
}

pub(super) fn run(
    state: &ServerState,
    websocket: &mut ObservedWebSocket<'_>,
    pending_request: &mut Option<String>,
    execute: impl FnOnce(&mut ObservedWebSocket<'_>, &mut DisconnectMonitor) -> TurnResult,
) -> TurnResult {
    let control = Arc::new(Control {
        previous_id: websocket.last_response_id.clone(),
        ..Control::default()
    });
    let request_id = websocket
        .observation
        .headers(Default::default())
        .get("X-EMP-Request-ID")
        .cloned();
    let (sender, receiver) = oneshot::channel();
    let mut monitor = DisconnectMonitor::from_signal(receiver, Arc::clone(&control.interrupted));
    let reader = websocket.inner.reader_task();
    let diagnostics = Arc::clone(&state.backend.diagnostics);
    let task_control = Arc::clone(&control);
    let (stop, mut stopped) = oneshot::channel::<()>();
    let worker = websocket.inner.runtime.spawn(async move {
        let mut sender = Some(sender);
        let mut pending = None;
        loop {
            let event = tokio::select! {
                biased;
                _ = &mut stopped => break,
                event = reader.read() => event,
            };
            let text = match reader.finish_read(event).await {
                DownstreamEvent::Text(text) => text,
                DownstreamEvent::Control => continue,
                DownstreamEvent::Closed => {
                    let _ = sender.take().unwrap().send(());
                    break;
                }
            };
            let value = serde_json::from_str::<Value>(&text).ok();
            if value.as_ref().is_none_or(|value| value["type"] != "response.interrupt") {
                pending = Some(text);
                break;
            }
            let value = value.unwrap();
            let (valid, previous, terminal) = {
                let response = task_control.response.lock().await;
                let id = value["response_id"].as_str();
                let mode_matches = value["mode"] == "discard_partial_items";
                let valid = id.is_some_and(|id| Some(id) == response.id.as_deref()) && mode_matches;
                let previous = id.is_some_and(|id| Some(id) == task_control.previous_id.as_deref()) && mode_matches;
                if valid && !response.terminal {
                    task_control.interrupted.store(true, Ordering::Release);
                }
                (valid, previous, response.terminal)
            };
            let journal = Arc::clone(&diagnostics);
            let fields = json!({
                "type":"response.interrupt", "accepted":valid || previous, "late":terminal || previous,
                "request_id":request_id,
                "error_origin":if valid || previous {"none"} else {"emp"},
                "error_code":if valid || previous {""} else {"invalid_request"},
                "response_id":value["response_id"].as_str().filter(|id| id.len() <= 128)
            });
            let _ = tokio::task::spawn_blocking(move || {
                journal.journal.event("info", "websocket_control", &fields);
            }).await;
            if !valid && !previous {
                let _ = reader.writer.send_json(&crate::api::failure_response::emp_websocket_error(400, "invalid_request", "interrupt must target the active response and use discard_partial_items")).await;
            } else if valid && !terminal {
                let _ = sender.take().unwrap().send(());
                break;
            }
        }
        (pending, sender)
    });
    websocket.control = Some(Arc::clone(&control));
    let result = execute(websocket, &mut monitor);
    drop(stop);
    let (pending, _sender) = websocket
        .inner
        .runtime
        .block_on(worker)
        .expect("websocket control reader");
    *pending_request = pending;
    let result = if let Some(event) = websocket.inner.runtime.block_on(control.incomplete()) {
        if websocket.send_json(&event).is_ok() {
            TurnResult::Finished
        } else {
            TurnResult::Closed
        }
    } else {
        result
    };
    websocket.control = None;
    result
}
