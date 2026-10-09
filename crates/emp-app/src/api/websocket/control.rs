//! Framed control ownership while a turn uses HTTP or the single-step CLI.
//! One bounded next request is retained; cancellation reuses the I/O race.
use super::{ObservedWebSocket, TurnResult};
use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use emp_transport::{WebSocketConnection, WebSocketPoll};
use polling::{Event, Events, Poller};
use serde_json::{Value, json};
use std::net::TcpStream;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

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
    pub(super) fn send(
        &self,
        websocket: &mut WebSocketConnection<'_, TcpStream>,
        event: &Value,
    ) -> Result<bool, emp_transport::WebSocketError> {
        let mut response = self.response.lock().unwrap();
        let interruption = event["type"] == "response.incomplete"
            && event["response"]["incomplete_details"]["reason"] == "interrupted";
        if self.interrupted.load(Ordering::Acquire) && !interruption {
            return Ok(false); // discard_partial_items: no further output is delivered.
        }
        websocket.send_json(event)?;
        if event["type"] == "response.created" {
            response.id = event["response"]["id"].as_str().map(str::to_owned);
        }
        response.terminal |= crate::services::events::terminal_stream_event(event);
        response.interruption_written |= interruption;
        Ok(true)
    }

    fn incomplete(&self) -> Option<Value> {
        let response = self.response.lock().unwrap();
        if !self.interrupted.load(Ordering::Acquire) || response.interruption_written {
            return None;
        }
        Some(json!({"type":"response.incomplete", "response":{
            "id":response.id.as_deref()?, "object":"response", "status":"incomplete",
            "incomplete_details":{"reason":"interrupted"}, "output":[], "end_turn":false
        }}))
    }
}

struct Readiness {
    socket: TcpStream,
    poller: Arc<Poller>,
}
impl Readiness {
    fn new(socket: &TcpStream) -> std::io::Result<Self> {
        let socket = socket.try_clone()?;
        let poller = Arc::new(Poller::new()?);
        // SAFETY: the owned socket is unregistered before it is dropped.
        unsafe {
            poller.add(&socket, Event::readable(0))?;
        }
        Ok(Self { socket, poller })
    }
}
impl Drop for Readiness {
    fn drop(&mut self) {
        let _ = self.poller.delete(&self.socket);
    }
}

struct Stop<'a>(&'a AtomicBool, &'a Poller);
impl Drop for Stop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        let _ = self.1.notify();
    }
}

pub(super) fn run(
    state: &ServerState,
    websocket: &mut ObservedWebSocket<'_, '_>,
    socket: &TcpStream,
    pending_request: &mut Option<String>,
    execute: impl FnOnce(&mut ObservedWebSocket<'_, '_>, &mut DisconnectMonitor) -> TurnResult,
) -> TurnResult {
    let Ok(mut socket) = socket.try_clone() else {
        return TurnResult::Closed;
    };
    let Ok(readiness) = Readiness::new(&socket) else {
        return TurnResult::Closed;
    };
    let control = Arc::new(Control {
        previous_id: websocket.last_response_id.clone(),
        ..Control::default()
    });
    let request_id = websocket
        .observation
        .headers(Default::default())
        .get("X-EMP-Request-ID")
        .cloned();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let Ok(mut monitor) =
        DisconnectMonitor::from_signal(receiver, Arc::clone(&control.interrupted))
    else {
        return TurnResult::Closed;
    };
    let Ok(mut reader) = websocket.inner.take_reader(&mut socket) else {
        return TurnResult::Closed;
    };
    websocket.control = Some(Arc::clone(&control));
    let stopped = AtomicBool::new(false);
    let result = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut sender = Some(sender);
            let mut pending = None;
            let mut events = Events::with_capacity(std::num::NonZeroUsize::new(1).unwrap());
            let mut ready = true; // includes frames prefetched by the previous turn.
            while !stopped.load(Ordering::Acquire) {
                if !ready {
                    events.clear();
                    match readiness.poller.wait(&mut events, None) {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                        Ok(_) => {},
                    }
                    if stopped.load(Ordering::Acquire) { break; }
                    ready = events.iter().any(|event| event.readable);
                    if !ready { continue; }
                }
                match reader.poll_text_ready() {
                    Ok(WebSocketPoll::Pending) => {
                        ready = false;
                        if readiness.poller.modify(&readiness.socket, Event::readable(0)).is_err() { break; }
                    }
                    Ok(WebSocketPoll::Ping(payload)) => {
                        if reader.send_pong(&payload).is_err() { break; }
                    }
                    Ok(WebSocketPoll::Text(text)) => {
                        let value = serde_json::from_str::<Value>(&text).ok();
                        if value.as_ref().is_none_or(|value| value["type"] != "response.interrupt") {
                            pending = Some(text);
                            break;
                        }
                        let value = value.unwrap();
                        let response = control.response.lock().unwrap();
                        let matches = value["response_id"].as_str().is_some_and(|id| Some(id) == response.id.as_deref());
                        let valid = matches && value["mode"] == "discard_partial_items";
                        let previous = value["response_id"].as_str().is_some_and(|id| Some(id) == control.previous_id.as_deref())
                            && value["mode"] == "discard_partial_items";
                        let terminal = response.terminal;
                        if valid && !terminal {
                            control.interrupted.store(true, Ordering::Release);
                        }
                        drop(response);
                        state.backend.diagnostics.journal.event("info", "websocket_control", &json!({
                            "type":"response.interrupt", "accepted":valid || previous, "late":terminal || previous,
                            "request_id":request_id,
                            "error_origin":if valid || previous {"none"} else {"emp"},
                            "error_code":if valid || previous {""} else {"invalid_request"},
                            "response_id":value["response_id"].as_str().filter(|id| id.len() <= 128)
                        }));
                        if !valid && !previous {
                            let _ = reader.send_json(&crate::api::failure_response::emp_websocket_error(400, "invalid_request", "interrupt must target the active response and use discard_partial_items"));
                        } else if valid && !terminal {
                            let _ = sender.take().unwrap().send(());
                            break;
                        }
                    }
                    Ok(WebSocketPoll::Closed { .. }) => {
                        let _ = sender.take().unwrap().send(());
                        break;
                    }
                    Err(error) => {
                        reader.close(error.close_code(), &error.to_string());
                        let _ = sender.take().unwrap().send(());
                        break;
                    }
                }
            }
            if pending.is_none() && !stopped.load(Ordering::Acquire)
                && let Some(sender) = sender.take()
            { let _ = sender.send(()); }
            // Retain the sender when a next request is queued: its Drop must not
            // signal a disconnect while inference is still running.
            (reader, pending, sender)
        });
        let stop = Stop(&stopped, &readiness.poller);
        let result = execute(websocket, &mut monitor);
        drop(stop);
        let (reader, pending, _sender) = worker.join().expect("websocket control reader");
        if websocket.inner.reclaim_reader(reader).is_err() {
            return TurnResult::Closed;
        }
        *pending_request = pending;
        result
    });
    let result = if let Some(event) = control.incomplete() {
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
