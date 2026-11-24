use crate::api::failure_response::pre_output_failure_response;
use crate::api::failure_response::pre_output_router_error_response_for_route;
use crate::api::failure_response::stream_failure_value_for_route;
use crate::app::ServerState;
use crate::http::response::SECURITY_HEADERS;
use crate::http::response::is_security_header;
use crate::server::http::{error::HttpReply, inference::BodyOutput};
use crate::services::disconnect::DisconnectMonitor;
use crate::services::disconnect::DisconnectRace;
use crate::services::events::sse_frame;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::native;
use crate::services::observation::request::RequestObservation;
use crate::services::providers::persist_protocol_observation;
use crate::util::random_hex;
use emp_domain::ResolvedRoute;
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
    for (name, value) in stream_headers(headers) {
        stream.write_all(name.as_bytes())?;
        stream.write_all(b": ")?;
        stream.write_all(value.as_bytes())?;
        stream.write_all(b"\r\n")?;
    }
    stream.write_all(b"Connection: close\r\n\r\n")
}

fn stream_headers(headers: &BTreeMap<String, String>) -> impl Iterator<Item = (&str, &str)> {
    headers.iter().filter_map(|(name, value)| {
        (!is_security_header(name)
            && ![
                "content-type",
                "content-length",
                "connection",
                "cache-control",
            ]
            .iter()
            .any(|header| name.eq_ignore_ascii_case(header)))
        .then_some((name.as_str(), value.as_str()))
    })
}

pub(crate) enum ReplySink<'a> {
    Legacy(&'a mut TcpStream),
    Candidate(&'a mut BodyOutput),
}

impl ReplySink<'_> {
    pub(crate) fn socket(&self) -> &TcpStream {
        match self {
            Self::Legacy(stream) => stream,
            Self::Candidate(output) => output.socket(),
        }
    }

    fn head(&mut self, headers: &BTreeMap<String, String>) -> std::io::Result<()> {
        match self {
            Self::Legacy(stream) => write_stream_head_with_headers(stream, headers),
            Self::Candidate(output) => {
                output.head(200, "text/event-stream", stream_headers(headers))
            }
        }
    }

    pub(crate) fn aero_head(&mut self, status: u16, content_type: &str) -> std::io::Result<()> {
        match self {
            Self::Legacy(stream) => {
                let status_text = crate::http::response::status_text(status);
                write!(
                    stream,
                    "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nConnection: close\r\nCache-Control: no-cache\r\n"
                )?;
                stream.write_all(SECURITY_HEADERS)?;
                stream.write_all(b"\r\n")
            }
            Self::Candidate(output) => output.head(status, content_type, std::iter::empty()),
        }
    }

    pub(crate) fn frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        match self {
            Self::Legacy(stream) => stream.write_all(frame),
            Self::Candidate(output) => output.frame(frame),
        }
    }

    fn frames(&mut self, frames: &[Vec<u8>]) -> std::io::Result<()> {
        match self {
            Self::Legacy(stream) => write_stream_frames(stream, frames),
            Self::Candidate(output) => {
                for frame in frames {
                    output.frame(frame)?;
                }
                Ok(())
            }
        }
    }
}

fn write_stream_frames(stream: &mut TcpStream, frames: &[Vec<u8>]) -> std::io::Result<()> {
    for frame in frames {
        stream.write_all(frame)?;
    }
    stream.flush()
}

/// Records downstream delivery while returning the writer's original I/O results.
pub(crate) struct ObservedSse<'a, 'socket> {
    sink: &'a mut ReplySink<'socket>,
    observation: &'a mut RequestObservation,
    codex: bool,
}
impl<'a, 'socket> ObservedSse<'a, 'socket> {
    pub(crate) fn new(
        sink: &'a mut ReplySink<'socket>,
        observation: &'a mut RequestObservation,
        incoming: &BTreeMap<String, String>,
    ) -> Self {
        Self {
            sink,
            observation,
            codex: crate::services::events::codex_client(incoming),
        }
    }
    pub(crate) fn head(&mut self) -> std::io::Result<()> {
        self.observation
            .written(self.sink.head(&BTreeMap::new()), false)
    }
    fn head_with_headers(&mut self, headers: &BTreeMap<String, String>) -> std::io::Result<()> {
        self.observation.written(self.sink.head(headers), false)
    }
    pub(crate) fn frames(&mut self, frames: &[Vec<u8>], last_event: &str) -> std::io::Result<()> {
        let result = self.write_frames(frames, last_event, false);
        self.observation.event_type_written(last_event, result)
    }

    pub(crate) fn event(&mut self, event: &Value, frames: &[Vec<u8>]) -> std::io::Result<()> {
        let result = self.write_frames(frames, event["type"].as_str().unwrap_or_default(), false);
        self.observation.event_written(event, result)
    }

    fn upstream_event(&mut self, event: &Value, frames: &[Vec<u8>]) -> std::io::Result<()> {
        let result = self.write_frames(frames, event["type"].as_str().unwrap_or_default(), true);
        self.observation.upstream_event_written(event, result)
    }

    fn write_frames(
        &mut self,
        frames: &[Vec<u8>],
        event_type: &str,
        upstream: bool,
    ) -> std::io::Result<()> {
        if !matches!(event_type, "response.failed" | "error") {
            return self.sink.frames(frames);
        }
        let formatted = frames
            .iter()
            .map(|frame| {
                self.observation
                    .errors()
                    .frame(frame, upstream)
                    .unwrap_or_else(|| frame.clone())
            })
            .collect::<Vec<_>>();
        self.sink.frames(&formatted)
    }
}

pub(crate) fn serve_external_stream(
    downstream: ObservedSse<'_, '_>,
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), HttpReply> {
    let mut monitor = DisconnectMonitor::start(downstream.sink.socket()).ok();
    let (upstream, candidate, usage) = match crate::services::external::open_stream(
        state,
        route,
        body,
        incoming,
        ids,
        monitor.as_mut(),
    ) {
        Ok(opened) => opened,
        Err(error) => {
            return crate::api::failure_response::external_open_error(error).map_or(Ok(()), Err);
        }
    };
    let completed = relay_stream(
        RelayContext {
            downstream,
            state,
            route: &candidate,
            body,
            response_headers: &BTreeMap::new(),
            usage,
        },
        ExternalRelay { upstream },
        monitor,
    )?;
    if completed {
        persist_protocol_observation(state, &candidate);
    }
    Ok(())
}

pub(crate) fn serve_native_stream(
    downstream: ObservedSse<'_, '_>,
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), HttpReply> {
    let monitor = DisconnectMonitor::start(downstream.sink.socket()).ok();
    let (upstream, monitor) = match monitor {
        Some(mut monitor) => match native::open_stream_cancellable(
            state,
            route,
            config,
            body,
            incoming,
            ids,
            &mut monitor,
        )
        .map_err(crate::api::native_response::error_reply)?
        {
            native::CancellableNativeStreamOpen::Opened(upstream) => (*upstream, Some(monitor)),
            native::CancellableNativeStreamOpen::Disconnected => return Ok(()),
        },
        None => (
            native::open_stream_result(state, route, config, body, incoming, ids, None)
                .map_err(crate::api::native_response::error_reply)?,
            None,
        ),
    };
    let (upstream, usage) = upstream;
    let response_headers = upstream.headers.clone();
    relay_stream(
        RelayContext {
            downstream,
            state,
            route,
            body,
            response_headers: &response_headers,
            usage,
        },
        NativeRelay { upstream },
        monitor,
    )
    .map(|_| ())
}

enum RelayEvent {
    /// End of stream without a terminal event.
    Incomplete,
    Disconnected,
    Failure(RouterError),
    SerializationFailure,
    Event {
        frame: Vec<u8>,
        body: Value,
    },
}

trait RelaySource {
    fn observation(&self) -> &emp_router::model_observation::ModelObservation;
    fn next(
        &mut self,
        runtime: &tokio::runtime::Runtime,
        monitor: Option<&mut DisconnectMonitor>,
    ) -> RelayEvent;
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

struct RelayContext<'a, 'socket> {
    downstream: ObservedSse<'a, 'socket>,
    state: &'a ServerState,
    route: &'a ResolvedRoute,
    body: &'a Value,
    response_headers: &'a BTreeMap<String, String>,
    usage: crate::services::request_outcome::RequestOutcome<'a>,
}

fn relay_stream(
    context: RelayContext<'_, '_>,
    mut source: impl RelaySource,
    monitor: Option<DisconnectMonitor>,
) -> Result<bool, HttpReply> {
    let RelayContext {
        mut downstream,
        state,
        route,
        body,
        response_headers,
        mut usage,
    } = context;
    let mut monitor = monitor.or_else(|| DisconnectMonitor::start(downstream.sink.socket()).ok());
    let runtime = &state.backend.transport.runtime;
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let event = source.next(runtime, monitor.as_mut());
        usage.upstream_observation(source.observation());
        let terminal =
            matches!(&event, RelayEvent::Event { body, .. } if terminal_stream_event(body));
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
                return Err(HttpReply::error(500, "internal server error", None));
            }
            RelayEvent::Event {
                frame,
                body: event_body,
            } => {
                usage.observe(&event_body);
                crate::services::context::record_event(state, route, body, &event_body);
                let frame = if !downstream.codex
                    && crate::services::events::codex_event(
                        event_body["type"].as_str().unwrap_or_default(),
                    ) {
                    b": keepalive\n\n".to_vec()
                } else {
                    frame
                };
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
                    return Err(HttpReply::error(
                        502,
                        "EMP could not parse the upstream response stream.",
                        Some("pre_output_buffer_limit"),
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

pub(crate) fn serve_generated_response(
    mut downstream: ObservedSse<'_, '_>,
    response_value: Value,
    ids: &ProjectionIds,
) -> Result<(), HttpReply> {
    let events = emp_router::response_json_stream_events(response_value, ids, false)
        .map_err(crate::api::failure_response::router_error_reply)?;
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
