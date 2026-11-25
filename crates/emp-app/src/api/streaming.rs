use crate::api::failure_response::pre_output_failure_response;
use crate::api::failure_response::pre_output_router_error_response_for_route;
use crate::api::failure_response::stream_failure_value_for_route;
use crate::app::ServerState;
use crate::http::response::is_security_header;
use crate::server::http::{error::HttpReply, stream::BodyOutput};
use crate::services::context;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::disconnect::DisconnectRace;
use crate::services::events::sse_frame;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::native;
use crate::services::observation::request::RequestObservation;
use crate::services::providers::save_learned_protocol;
use crate::services::request_outcome::RequestOutcome;
use crate::util::random_hex;
use axum::body::Bytes;
use emp_domain::ResolvedRoute;
use emp_router::ExternalStream;
use emp_router::ProjectionIds;
use emp_router::RouterError;
use emp_router::native_http::NativeStream;
use emp_state::{config::LearnedProtocol, observed_at_now};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_PRE_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;

const MAX_PRE_OUTPUT_BUFFER_EVENTS: usize = 256;

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

pub(crate) struct ObservedSse<'a, 'state> {
    sink: &'a mut BodyOutput<'state>,
    observation: &'a mut RequestObservation,
    codex: bool,
}
impl<'a, 'state> ObservedSse<'a, 'state> {
    pub(crate) fn new(
        sink: &'a mut BodyOutput<'state>,
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
        self.observation.written(
            self.sink.head(200, "text/event-stream", std::iter::empty()),
            false,
        )
    }
    fn head_with_headers(&mut self, headers: &BTreeMap<String, String>) -> std::io::Result<()> {
        self.observation.written(
            self.sink
                .head(200, "text/event-stream", stream_headers(headers)),
            false,
        )
    }
    pub(crate) fn frames(&mut self, frames: &[Vec<u8>], last_event: &str) -> std::io::Result<()> {
        self.write_frames(frames, None, last_event, false)
    }

    pub(crate) fn event(&mut self, event: &Value, frames: &[Vec<u8>]) -> std::io::Result<()> {
        self.write_frames(
            frames,
            Some(event),
            event["type"].as_str().unwrap_or_default(),
            false,
        )
    }

    fn upstream_event(&mut self, event: &Value, frames: &[Vec<u8>]) -> std::io::Result<()> {
        self.write_frames(
            frames,
            Some(event),
            event["type"].as_str().unwrap_or_default(),
            true,
        )
    }

    fn write_frames(
        &mut self,
        frames: &[Vec<u8>],
        event: Option<&Value>,
        event_type: &str,
        upstream: bool,
    ) -> std::io::Result<()> {
        self.sink.flush_frame()?;
        let diagnostics = self.observation.frame_event(event, event_type, upstream);
        let format_error = matches!(event_type, "response.failed" | "error");
        let bytes = |frame: &[u8]| {
            format_error
                .then(|| self.observation.errors().frame(frame, upstream))
                .flatten()
                .map_or_else(|| Bytes::copy_from_slice(frame), Bytes::from)
        };
        if let Some((last, preceding)) = frames.split_last() {
            for frame in preceding {
                self.sink.write_frame(bytes(frame), None)?;
            }
            self.sink.write_frame(bytes(last), Some(diagnostics))
        } else {
            diagnostics.flushed();
            Ok(())
        }
    }
}

pub(crate) fn serve_external_stream<'a>(
    mut downstream: ObservedSse<'_, 'a>,
    state: &'a ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), HttpReply> {
    let mut monitor = downstream.sink.disconnect().ok();
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
    let mut learned = None;
    let result = relay_stream(
        &mut downstream,
        RelayContext {
            state,
            route: &candidate,
            response_headers: BTreeMap::new(),
            usage,
        },
        ExternalRelay { upstream },
        monitor,
        |usage, event| {
            if event["type"] == "response.completed" {
                learned = LearnedProtocol::from_route(route, candidate.protocol, observed_at_now());
            }
            usage.observe(event);
            context::record_event(state, &candidate, body, event);
        },
    );
    if learned.is_some() {
        downstream
            .sink
            .after_handoff(move || save_learned_protocol(state, learned));
    }
    result
}

pub(crate) fn serve_native_stream<'a>(
    mut downstream: ObservedSse<'_, 'a>,
    state: &'a ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
) -> Result<(), HttpReply> {
    let monitor = downstream.sink.disconnect().ok();
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
            native::open_stream_result(state, route, config, body, incoming, ids)
                .map_err(crate::api::native_response::error_reply)?,
            None,
        ),
    };
    let (upstream, usage) = upstream;
    let response_headers = upstream.headers.clone();
    relay_stream(
        &mut downstream,
        RelayContext {
            state,
            route,
            response_headers,
            usage,
        },
        NativeRelay { upstream },
        monitor,
        |usage, event| {
            usage.observe(event);
            if let Some(success) = context::outcome(event)
                && let Some(payload) = context::native_payload(
                    state,
                    route,
                    body,
                    native::plaintext_collaboration(config),
                )
            {
                context::record_payload(state, route, &payload, success);
            }
        },
    )
}

enum RelayEvent {
    Incomplete,
    Disconnected,
    Failure(RouterError),
    SerializationFailure,
    Event { frame: Vec<u8>, body: Value },
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

struct RelayContext<'a> {
    state: &'a ServerState,
    route: &'a ResolvedRoute,
    response_headers: BTreeMap<String, String>,
    usage: RequestOutcome<'a>,
}

fn relay_stream(
    downstream: &mut ObservedSse<'_, '_>,
    context: RelayContext<'_>,
    mut source: impl RelaySource,
    monitor: Option<DisconnectMonitor>,
    mut observe: impl FnMut(&mut RequestOutcome<'_>, &Value),
) -> Result<(), HttpReply> {
    let RelayContext {
        state,
        route,
        response_headers,
        mut usage,
    } = context;
    let mut monitor = monitor.or_else(|| downstream.sink.disconnect().ok());
    let runtime = &state.backend.transport.runtime;
    let mut pending = Vec::<Vec<u8>>::new();
    let mut pending_bytes = 0_usize;
    let mut started = false;
    loop {
        let event = source.next(runtime, monitor.as_mut());
        usage.upstream_observation(source.observation());
        match event {
            RelayEvent::Disconnected => {
                usage.disconnected();
                if started {
                    let _ = downstream.sink.finish_producing();
                }
                break Ok(());
            }
            RelayEvent::Incomplete => {
                usage.status(502, "stream_incomplete");
                if started {
                    let _ = downstream.sink.finish_producing();
                }
                break Ok(());
            }
            RelayEvent::Failure(error) => {
                usage.router_error(&error);
                if !started {
                    break Err(pre_output_router_error_response_for_route(&error, route));
                }
                let response_id = match random_hex(16) {
                    Ok(value) => format!("resp_{value}"),
                    Err(_) => break Ok(()),
                };
                let failure = stream_failure_value_for_route(&error, &response_id, route, true);
                if let Ok(frame) = sse_frame("response.failed", &failure) {
                    let _ = downstream.event(&failure, &[frame]);
                }
                break Ok(());
            }
            RelayEvent::SerializationFailure => {
                usage.status(500, "stream_error");
                break Err(HttpReply::error(500, "internal server error", None));
            }
            RelayEvent::Event {
                frame,
                body: event_body,
            } => {
                observe(&mut usage, &event_body);
                let terminal = terminal_stream_event(&event_body);
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
                        break Ok(());
                    }
                    if terminal {
                        break Ok(());
                    }
                    continue;
                }
                if failed && let Some(response) = pre_output_failure_response(&event_body, route) {
                    break Err(response);
                }
                let (output_emitted, tool_activity) = stream_event_activity(&event_body);
                pending_bytes = pending_bytes.saturating_add(frame.len());
                pending.push(frame);
                if pending.len() > MAX_PRE_OUTPUT_BUFFER_EVENTS
                    || pending_bytes > MAX_PRE_OUTPUT_BUFFER_BYTES
                {
                    break Err(HttpReply::error(
                        502,
                        "EMP could not parse the upstream response stream.",
                        Some("pre_output_buffer_limit"),
                    ));
                }
                if output_emitted || tool_activity || terminal {
                    let head_ok = if response_headers.is_empty() {
                        downstream.head().is_ok()
                    } else {
                        downstream.head_with_headers(&response_headers).is_ok()
                    };
                    if !head_ok || downstream.upstream_event(&event_body, &pending).is_err() {
                        break Ok(());
                    }
                    started = true;
                    pending.clear();
                    if terminal {
                        break Ok(());
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
