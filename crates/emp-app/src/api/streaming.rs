//! Downstream SSE delivery and cancellation.

use crate::app::ServerState;
use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::services::events::sse_frame;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::failures::external_retry_delay;
use crate::services::failures::pre_output_failure_response;
use crate::services::failures::pre_output_router_error_response;
use crate::services::failures::route_resolution_response;
use crate::services::failures::stream_failure_value;
use crate::services::native;
use crate::services::providers::persist_protocol_observation;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_router::ExternalRouter;
use emp_router::ExternalStream;
use emp_router::ProjectionIds;
use emp_router::RouterError;
use emp_router::StreamResponseEvent;
use emp_router::native_http::NativeStream;
use emp_router::protocol_candidates;
use emp_transport::protocol_fallback_allowed;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

const MAX_PRE_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;

const MAX_PRE_OUTPUT_BUFFER_EVENTS: usize = 256;

struct DisconnectMonitor {
    disconnected: tokio::sync::oneshot::Receiver<()>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl DisconnectMonitor {
    fn start(stream: &TcpStream) -> std::io::Result<Self> {
        let probe = stream.try_clone()?;
        probe.set_read_timeout(Some(Duration::from_millis(50)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (sender, disconnected) = tokio::sync::oneshot::channel();
        let worker = thread::spawn(move || {
            let mut byte = [0_u8; 1];
            while !worker_stop.load(Ordering::Acquire) {
                match probe.peek(&mut byte) {
                    Ok(0) => {
                        let _ = sender.send(());
                        return;
                    }
                    Ok(_) => thread::sleep(Duration::from_millis(10)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => {
                        let _ = sender.send(());
                        return;
                    }
                }
            }
        });
        Ok(Self {
            disconnected,
            stop,
            worker: Some(worker),
        })
    }

    async fn next_event(&mut self, stream: &mut ExternalStream) -> StreamPoll {
        tokio::select! {
            result = stream.next_event() => StreamPoll::Event(result),
            _ = &mut self.disconnected => StreamPoll::Disconnected,
        }
    }

    async fn next_native_event(&mut self, stream: &mut NativeStream) -> NativeStreamPoll {
        tokio::select! {
            result = stream.next_event() => NativeStreamPoll::Event(result),
            _ = &mut self.disconnected => NativeStreamPoll::Disconnected,
        }
    }
}

impl Drop for DisconnectMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum StreamPoll {
    Event(Result<Option<StreamResponseEvent>, RouterError>),
    Disconnected,
}

enum NativeStreamPoll {
    Event(Result<Option<emp_router::native_http::NativeStreamEvent>, RouterError>),
    Disconnected,
}

pub(crate) fn write_stream_head(stream: &mut TcpStream) -> std::io::Result<()> {
    write_stream_head_with_headers(stream, &BTreeMap::new())
}

fn write_stream_head_with_headers(
    stream: &mut TcpStream,
    headers: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    stream.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n",
    )?;
    for (name, value) in headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "content-length" | "connection" | "cache-control"
        ) {
            continue;
        }
        stream.write_all(name.as_bytes())?;
        stream.write_all(b": ")?;
        stream.write_all(value.as_bytes())?;
        stream.write_all(b"\r\n")?;
    }
    stream.write_all(b"Connection: close\r\n\r\n")
}

pub(crate) fn write_stream_frames(
    stream: &mut TcpStream,
    frames: &[Vec<u8>],
) -> std::io::Result<()> {
    for frame in frames {
        stream.write_all(frame)?;
    }
    stream.flush()
}

pub(crate) fn serve_external_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let router = ExternalRouter::new(&state.backend.transport.client);
    let candidates = protocol_candidates(route);
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(route_resolution_response)?;
        for attempt in 0..2 {
            match state
                .backend
                .transport
                .runtime
                .block_on(router.open_stream(&candidate, body, incoming, ids))
            {
                Ok(upstream) => {
                    let completed = relay_external_stream(downstream, state, upstream)?;
                    if completed {
                        crate::services::context::record(state, &candidate, body, true);
                        persist_protocol_observation(state, &candidate);
                    }
                    return Ok(());
                }
                Err(error) => {
                    if error.error_class() == emp_transport::FailureClass::ContextLengthExceeded {
                        crate::services::context::record(state, &candidate, body, false);
                    }
                    if let Some(delay) = external_retry_delay(&error, attempt, &candidate) {
                        thread::sleep(delay);
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && protocol_fallback_allowed(error.status(), false, false)
                    {
                        continue 'candidate;
                    }
                    return Err(pre_output_router_error_response(&error));
                }
            }
        }
    }
    Err(json_error_response(
        503,
        status_text(503),
        "provider protocol is unsupported",
        Some("router_error"),
        &[],
    ))
}

pub(crate) fn serve_native_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let upstream = native::open_stream(
        state,
        route,
        config,
        body.as_object().expect("validated request object"),
        incoming,
        ids,
    )?;
    relay_native_stream(downstream, state, upstream).map(|_| ())
}

fn relay_native_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    mut upstream: NativeStream,
) -> Result<bool, Vec<u8>> {
    let response_headers = upstream.headers.clone();
    let mut monitor = DisconnectMonitor::start(downstream).ok();
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let polled = match monitor.as_mut() {
            Some(monitor) => state
                .backend
                .transport
                .runtime
                .block_on(monitor.next_native_event(&mut upstream)),
            None => NativeStreamPoll::Event(
                state
                    .backend
                    .transport
                    .runtime
                    .block_on(upstream.next_event()),
            ),
        };
        let event = match polled {
            NativeStreamPoll::Disconnected => return Ok(false),
            NativeStreamPoll::Event(Ok(Some(event))) => event,
            NativeStreamPoll::Event(Ok(None)) => return Ok(false),
            NativeStreamPoll::Event(Err(error)) if !started => {
                return Err(pre_output_router_error_response(&error));
            }
            NativeStreamPoll::Event(Err(error)) => {
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => return Ok(false),
                };
                let failure = stream_failure_value(&error, &response_id);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = write_stream_frames(downstream, &[frame]);
                }
                return Ok(false);
            }
        };
        let terminal = terminal_stream_event(&event.body);
        let completed = event.event == "response.completed";
        let failed = matches!(event.event.as_str(), "response.failed" | "error");
        let frame = event.frame;
        if started {
            if write_stream_frames(downstream, &[frame]).is_err() {
                return Ok(false);
            }
            if terminal {
                state.backend.transport.runtime.block_on(upstream.finish());
                return Ok(completed);
            }
            continue;
        }
        if failed {
            if let Some(response) = pre_output_failure_response(&event.body) {
                return Err(response);
            }
            if write_stream_head_with_headers(downstream, &response_headers).is_err()
                || write_stream_frames(downstream, &[frame]).is_err()
            {
                return Ok(false);
            }
            return Ok(false);
        }
        let (output_emitted, tool_activity) = stream_event_activity(&event.body);
        pending_bytes = pending_bytes.saturating_add(frame.len());
        pending.push(frame);
        if pending.len() > MAX_PRE_OUTPUT_BUFFER_EVENTS
            || pending_bytes > MAX_PRE_OUTPUT_BUFFER_BYTES
        {
            return Err(json_error_response(
                502,
                status_text(502),
                "EMP could not parse the upstream response stream.",
                Some("pre_output_buffer_limit"),
                &[],
            ));
        }
        if output_emitted || tool_activity || terminal {
            if write_stream_head_with_headers(downstream, &response_headers).is_err()
                || write_stream_frames(downstream, &pending).is_err()
            {
                return Ok(false);
            }
            started = true;
            pending.clear();
            if terminal {
                state.backend.transport.runtime.block_on(upstream.finish());
                return Ok(completed);
            }
        }
    }
}

fn relay_external_stream(
    downstream: &mut TcpStream,
    state: &ServerState,
    mut upstream: ExternalStream,
) -> Result<bool, Vec<u8>> {
    let mut monitor = DisconnectMonitor::start(downstream).ok();
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let polled = match monitor.as_mut() {
            Some(monitor) => state
                .backend
                .transport
                .runtime
                .block_on(monitor.next_event(&mut upstream)),
            None => StreamPoll::Event(
                state
                    .backend
                    .transport
                    .runtime
                    .block_on(upstream.next_event()),
            ),
        };
        let event = match polled {
            StreamPoll::Disconnected => return Ok(false),
            StreamPoll::Event(Ok(Some(event))) => event,
            StreamPoll::Event(Ok(None)) => return Ok(false),
            StreamPoll::Event(Err(error)) if !started => {
                return Err(pre_output_router_error_response(&error));
            }
            StreamPoll::Event(Err(error)) => {
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => return Ok(false),
                };
                let failure = stream_failure_value(&error, &response_id);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = write_stream_frames(downstream, &[frame]);
                }
                return Ok(false);
            }
        };
        let terminal = terminal_stream_event(&event.body);
        let completed =
            event.body.get("type").and_then(Value::as_str) == Some("response.completed");
        let failed = matches!(
            event.body.get("type").and_then(Value::as_str),
            Some("response.failed" | "error")
        );
        let frame = match sse_frame(&event.event, &event.body) {
            Ok(frame) => frame,
            Err(_) if !started => {
                return Err(json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                ));
            }
            Err(_) => return Ok(false),
        };
        if started {
            if write_stream_frames(downstream, &[frame]).is_err() {
                return Ok(false);
            }
            if terminal {
                return Ok(completed);
            }
            continue;
        }
        if failed {
            if let Some(response) = pre_output_failure_response(&event.body) {
                return Err(response);
            }
            if write_stream_head(downstream).is_err()
                || write_stream_frames(downstream, &[frame]).is_err()
            {
                return Ok(false);
            }
            return Ok(false);
        }
        let (output_emitted, tool_activity) = stream_event_activity(&event.body);
        pending_bytes = pending_bytes.saturating_add(frame.len());
        pending.push(frame);
        if pending.len() > MAX_PRE_OUTPUT_BUFFER_EVENTS
            || pending_bytes > MAX_PRE_OUTPUT_BUFFER_BYTES
        {
            return Err(json_error_response(
                502,
                status_text(502),
                "EMP could not parse the upstream response stream.",
                Some("pre_output_buffer_limit"),
                &[],
            ));
        }
        if output_emitted || tool_activity || terminal {
            if write_stream_head(downstream).is_err()
                || write_stream_frames(downstream, &pending).is_err()
            {
                return Ok(false);
            }
            started = true;
            pending.clear();
            if terminal {
                return Ok(completed);
            }
        }
    }
}
