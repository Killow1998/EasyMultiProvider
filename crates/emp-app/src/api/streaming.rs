//! Downstream SSE delivery and cancellation.

use crate::api::failure_response::pre_output_failure_response;
use crate::api::failure_response::pre_output_router_error_response_for_route;
use crate::api::failure_response::stream_failure_value_for_route;
use crate::app::ServerState;
use crate::http::response::SECURITY_HEADERS;
use crate::http::response::json_error_response;
use crate::http::response::status_text;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::disconnect::DisconnectRace;
use crate::services::events::sse_frame;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::native;
use crate::services::observation::request::RequestObservation;
use crate::services::providers::persist_protocol_observation;
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_router::ExternalStream;
use emp_router::ProjectionIds;
use emp_router::RouterError;
use emp_router::native_http::NativeStream;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::net::TcpStream;

const MAX_PRE_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;

const MAX_PRE_OUTPUT_BUFFER_EVENTS: usize = 256;

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
    stream.write_all(SECURITY_HEADERS)?;
    for (name, value) in headers {
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type"
                | "content-length"
                | "connection"
                | "cache-control"
                | "x-frame-options"
                | "content-security-policy"
                | "x-content-type-options"
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

/// A downstream writer and its passive receipt travel together. The writer
/// returns every original I/O result, leaving decisions to the existing relay.
pub(crate) struct ObservedSse<'a> {
    stream: &'a mut TcpStream,
    observation: &'a mut RequestObservation,
}
impl<'a> ObservedSse<'a> {
    pub(crate) fn new(stream: &'a mut TcpStream, observation: &'a mut RequestObservation) -> Self {
        Self {
            stream,
            observation,
        }
    }
    pub(crate) fn head(&mut self) -> std::io::Result<()> {
        self.observation
            .written(write_stream_head(self.stream), false)
    }
    fn head_with_headers(&mut self, headers: &BTreeMap<String, String>) -> std::io::Result<()> {
        self.observation
            .written(write_stream_head_with_headers(self.stream, headers), false)
    }
    pub(crate) fn frames(&mut self, frames: &[Vec<u8>], last_event: &str) -> std::io::Result<()> {
        self.observation
            .event_type_written(last_event, write_stream_frames(self.stream, frames))
    }
    fn event(&mut self, event: &Value, frames: &[Vec<u8>]) -> std::io::Result<()> {
        self.observation
            .event_written(event, write_stream_frames(self.stream, frames))
    }
    fn upstream_event(&mut self, event: &Value, frames: &[Vec<u8>]) -> std::io::Result<()> {
        self.observation
            .upstream_event_written(event, write_stream_frames(self.stream, frames))
    }
}

pub(crate) fn serve_external_stream(
    downstream: ObservedSse<'_>,
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let mut monitor = DisconnectMonitor::start(downstream.stream).ok();
    let (upstream, candidate) = match crate::services::external::open_stream(
        state,
        route,
        body,
        incoming,
        ids,
        monitor.as_mut(),
    ) {
        Ok(opened) => opened,
        Err(crate::services::external::ExternalRequestError::Disconnected) => return Ok(()),
        Err(error) => return Err(crate::api::failure_response::external_open_error(error)),
    };
    let completed = relay_external_stream(
        downstream, state, &candidate, body, incoming, upstream, monitor,
    )?;
    if completed {
        persist_protocol_observation(state, &candidate);
    }
    Ok(())
}

pub(crate) fn serve_native_stream(
    downstream: ObservedSse<'_>,
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let monitor = DisconnectMonitor::start(downstream.stream).ok();
    let (upstream, monitor) = match monitor {
        Some(mut monitor) => match native::open_stream_cancellable(
            state,
            route,
            config,
            body.as_object().expect("validated request object"),
            incoming,
            ids,
            &mut monitor,
        )
        .map_err(crate::api::native_response::error_response)?
        {
            native::CancellableNativeStreamOpen::Opened(upstream) => (*upstream, Some(monitor)),
            native::CancellableNativeStreamOpen::Disconnected => return Ok(()),
        },
        None => (
            native::open_stream_result(
                state,
                route,
                config,
                body.as_object().expect("validated request object"),
                incoming,
                ids,
            )
            .map_err(crate::api::native_response::error_response)?,
            None,
        ),
    };
    relay_native_stream(downstream, state, route, body, incoming, upstream, monitor).map(|_| ())
}

/// One relay state machine for every upstream flavor.
///
/// The two upstream types differ only in how a decoded event becomes an SSE
/// frame and whether the upstream wants a drain call on terminal; everything
/// else — disconnect racing, pre-output buffering, failure conversion,
/// header emission — is shared.
enum RelayEvent {
    /// End of stream without a terminal event.
    Incomplete,
    /// Downstream peer vanished mid-relay.
    Disconnected,
    /// Upstream failure before or after output flowed.
    Failure(RouterError),
    /// An SSE frame could not be produced for an event.
    SerializationFailure,
    /// A decodable upstream event with its wire frame.
    Event { frame: Vec<u8>, body: Value },
}

/// Decode the next upstream event for one relay turn.
trait RelaySource {
    fn observation(&self) -> &emp_router::model_observation::ModelObservation;
    fn next(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        monitor: Option<&mut DisconnectMonitor>,
    ) -> RelayEvent;
}

fn relay_terminal(event: &RelayEvent) -> bool {
    let RelayEvent::Event { body, .. } = event else {
        return false;
    };
    terminal_stream_event(body)
}

struct NativeRelay {
    upstream: NativeStream,
}

impl RelaySource for NativeRelay {
    fn observation(&self) -> &emp_router::model_observation::ModelObservation {
        &self.upstream.observation
    }
    fn next(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        monitor: Option<&mut DisconnectMonitor>,
    ) -> RelayEvent {
        match crate::services::disconnect::raced(runtime, monitor, self.upstream.next_event()) {
            DisconnectRace::Disconnected => RelayEvent::Disconnected,
            DisconnectRace::Ready(Ok(Some(event))) => RelayEvent::Event {
                frame: event.frame,
                body: event.body,
            },
            DisconnectRace::Ready(Ok(None)) => RelayEvent::Incomplete,
            DisconnectRace::Ready(Err(error)) => RelayEvent::Failure(error),
        }
    }
}

struct ExternalRelay {
    upstream: ExternalStream,
}

impl RelaySource for ExternalRelay {
    fn observation(&self) -> &emp_router::model_observation::ModelObservation {
        &self.upstream.observation
    }
    fn next(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        monitor: Option<&mut DisconnectMonitor>,
    ) -> RelayEvent {
        match crate::services::disconnect::raced(runtime, monitor, self.upstream.next_event()) {
            DisconnectRace::Disconnected => RelayEvent::Disconnected,
            DisconnectRace::Ready(Ok(Some(event))) => match sse_frame(&event.event, &event.body) {
                Ok(frame) => RelayEvent::Event {
                    frame,
                    body: event.body,
                },
                Err(_) => RelayEvent::SerializationFailure,
            },
            DisconnectRace::Ready(Ok(None)) => RelayEvent::Incomplete,
            DisconnectRace::Ready(Err(error)) => RelayEvent::Failure(error),
        }
    }
}

/// Shared relay inputs that do not change per upstream flavor.
struct RelayContext<'a> {
    downstream: ObservedSse<'a>,
    state: &'a ServerState,
    route: &'a ResolvedRoute,
    body: &'a Value,
    incoming: &'a BTreeMap<String, String>,
    response_headers: &'a BTreeMap<String, String>,
    usage_owner: Option<&'a str>,
    started_at: std::time::Instant,
}

fn relay_stream(
    context: RelayContext<'_>,
    mut source: impl RelaySource,
    monitor: Option<DisconnectMonitor>,
) -> Result<bool, Vec<u8>> {
    let RelayContext {
        mut downstream,
        state,
        route,
        body,
        incoming,
        response_headers,
        usage_owner,
        started_at,
    } = context;
    let mut usage = crate::services::request_outcome::RequestOutcome::new(
        state,
        route,
        body,
        incoming,
        usage_owner,
        "responses",
    )
    .started_at(started_at);
    let mut monitor = monitor.or_else(|| DisconnectMonitor::start(downstream.stream).ok());
    let runtime = &state.backend.transport.runtime;
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let event = source.next(runtime, monitor.as_mut());
        usage.upstream_observation(source.observation());
        let terminal = relay_terminal(&event);
        let completed = terminal
            && match &event {
                RelayEvent::Event { body, .. } => {
                    body.get("type").and_then(Value::as_str) == Some("response.completed")
                }
                _ => false,
            };
        match event {
            RelayEvent::Disconnected => {
                usage.disconnected();
                return Ok(false);
            }
            RelayEvent::Incomplete => {
                usage.status(502, "stream_incomplete");
                return Ok(false);
            }
            RelayEvent::Failure(error) => {
                usage.router_error(&error);
                if !started {
                    return Err(pre_output_router_error_response_for_route(&error, route));
                }
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => return Ok(false),
                };
                let failure = stream_failure_value_for_route(&error, &response_id, route, true);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = downstream.event(&failure, &[frame]);
                }
                return Ok(false);
            }
            RelayEvent::SerializationFailure => {
                usage.status(500, "stream_error");
                return Err(json_error_response(
                    500,
                    status_text(500),
                    "internal server error",
                    None,
                    &[],
                ));
            }
            RelayEvent::Event {
                frame,
                body: event_body,
            } => {
                usage.observe(&event_body);
                crate::services::context::record_event(state, route, body, &event_body);
                let failed = matches!(
                    event_body.get("type").and_then(Value::as_str),
                    Some("response.failed" | "error")
                );
                if started {
                    if downstream.upstream_event(&event_body, &[frame]).is_err() {
                        return Ok(false);
                    }
                    if terminal {
                        return Ok(completed);
                    }
                    continue;
                }
                if failed && let Some(response) = pre_output_failure_response(&event_body, route) {
                    return Err(response);
                }
                let (output_emitted, tool_activity) = stream_event_activity(&event_body);
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
                    let head_ok = if response_headers.is_empty() {
                        downstream.head().is_ok()
                    } else {
                        downstream.head_with_headers(response_headers).is_ok()
                    };
                    if !head_ok || downstream.upstream_event(&event_body, &pending).is_err() {
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
    }
}

fn relay_native_stream(
    downstream: ObservedSse<'_>,
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    upstream: NativeStream,
    monitor: Option<DisconnectMonitor>,
) -> Result<bool, Vec<u8>> {
    let response_headers = upstream.headers.clone();
    let started_at = upstream.request_started;
    let owner = upstream.usage_owner.clone();
    relay_stream(
        RelayContext {
            downstream,
            state,
            route,
            body,
            incoming,
            response_headers: &response_headers,
            usage_owner: owner.as_deref(),
            started_at,
        },
        NativeRelay { upstream },
        monitor,
    )
}

fn relay_external_stream(
    downstream: ObservedSse<'_>,
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    upstream: ExternalStream,
    monitor: Option<DisconnectMonitor>,
) -> Result<bool, Vec<u8>> {
    let started_at = upstream.request_started;
    relay_stream(
        RelayContext {
            downstream,
            state,
            route,
            body,
            incoming,
            response_headers: &BTreeMap::new(),
            usage_owner: None,
            started_at,
        },
        ExternalRelay { upstream },
        monitor,
    )
}

pub(crate) fn serve_generated_response(
    mut downstream: ObservedSse<'_>,
    response_value: Value,
    ids: &ProjectionIds,
) -> Result<(), Vec<u8>> {
    let events = emp_router::response_json_stream_events(response_value, ids, false)
        .map_err(crate::api::failure_response::router_error_response)?;
    if downstream.head().is_err() {
        return Ok(());
    }
    for event in events {
        let kind = event["type"].as_str().unwrap_or("message");
        let Ok(frame) = sse_frame(kind, &event) else {
            return Ok(());
        };
        if downstream.event(&event, &[frame]).is_err() {
            return Ok(());
        }
    }
    Ok(())
}
