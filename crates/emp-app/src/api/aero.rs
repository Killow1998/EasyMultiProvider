use crate::{
    app::ServerState,
    http::request::Request,
    server::http::stream::BodyOutput,
    server::http::{error::HttpReply, inference::InferenceOutcome},
    services::{
        disconnect::{DisconnectMonitor, DisconnectRace},
        gateway_clients::Caller,
        modules::aero::{CallLease, Relay},
        observation::request::{Phase, RequestObservation},
    },
};
use axum::body::Bytes;
use emp_transport::{HttpMethod, HttpResponse, SseFrame, SseJsonParser};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn selected_config(state: &ServerState, body: &Value) -> Option<Value> {
    let model = body["model"]
        .as_str()
        .filter(|model| model.starts_with("aero/"))?;
    let config = state.backend.configuration.snapshot().ok()?;
    crate::services::modules::aero::profile(&config, model)?;
    Some(config)
}

pub(super) enum RelayError {
    Disconnected,
    History(emp_context::HistoryError),
    Request(u16, &'static str, &'static str),
}

impl RelayError {
    pub(super) fn unavailable() -> Self {
        Self::Request(
            503,
            "aero_runtime_unavailable",
            "The Aero module could not start or connect.",
        )
    }
}

pub(super) struct Upstream {
    pub(super) response: HttpResponse,
    _relay: Relay,
}

pub(super) async fn await_module<F: std::future::Future>(
    state: &ServerState,
    monitor: &mut DisconnectMonitor,
    future: F,
) -> DisconnectRace<F::Output> {
    tokio::select! {
            biased;
            _ = state.wait_for_shutdown() => DisconnectRace::Disconnected,
            result = monitor.race(future) => result,
    }
}

pub(crate) fn relay(
    downstream: &mut BodyOutput,
    state: &ServerState,
    config: &Value,
    body: &Value,
    (request, incoming): (Request<'_>, &BTreeMap<String, String>),
    observation: &mut RequestObservation,
    compact: bool,
) -> InferenceOutcome {
    let monitor = downstream
        .disconnect()
        .map_err(|_| RelayError::unavailable());
    let upstream = monitor.and_then(|mut monitor| {
        state
            .backend
            .transport
            .runtime
            .block_on(open(
                state,
                config,
                body,
                (request, incoming),
                observation,
                (compact, |caller, authorization, lease| {
                    downstream.delegate(&state.backend.aero_callers, caller, authorization, lease)
                }),
                &mut monitor,
            ))
            .map(|upstream| (upstream, monitor))
    });
    let (mut upstream, mut monitor) = match upstream {
        Ok(upstream) => upstream,
        Err(RelayError::Disconnected) => return InferenceOutcome::Streamed,
        Err(RelayError::History(error)) => {
            return InferenceOutcome::Buffered(crate::api::history_response::history_http_error(
                &error,
                observation,
            ));
        }
        Err(RelayError::Request(status, code, message)) => {
            return InferenceOutcome::Buffered(HttpReply::error(status, message, Some(code)));
        }
    };
    let status = upstream.response.status();
    let content_type = upstream
        .response
        .header("content-type")
        .unwrap_or("application/json");
    let sse = content_type.starts_with("text/event-stream");
    if observation
        .written(
            downstream.head(status, content_type, std::iter::empty()),
            false,
        )
        .is_err()
    {
        return InferenceOutcome::Streamed;
    }
    let mut parser = sse.then(SseJsonParser::new);
    loop {
        let next = state
            .backend
            .transport
            .runtime
            .block_on(monitor.race(upstream.response.next_chunk()));
        match next {
            DisconnectRace::Ready(Ok(Some(chunk))) => {
                let mut events = Vec::new();
                if let Some(parser) = &mut parser {
                    let mut malformed = false;
                    for frame in parser.push_frame_results(&chunk) {
                        match frame {
                            Ok(SseFrame::Json(event)) => {
                                observation.observe_aero_terminal(&event);
                                events.push(event);
                            }
                            Ok(SseFrame::Done) => {}
                            Err(_) => malformed = true,
                        }
                    }
                    if malformed {
                        events.clear();
                    }
                }
                if downstream.flush_frame().is_err() {
                    break;
                }
                let diagnostics = observation.aero_chunk(events);
                if downstream
                    .write_frame(Bytes::from(chunk), Some(diagnostics))
                    .is_err()
                {
                    break;
                }
            }
            DisconnectRace::Ready(Ok(None)) => {
                if downstream.finish_producing().is_ok() && !sse {
                    observation.http_response(&json!(status), Ok(()));
                }
                break;
            }
            _ => {
                let _ = downstream.finish_producing();
                break;
            }
        }
    }
    InferenceOutcome::Streamed
}

pub(super) async fn open(
    state: &ServerState,
    config: &Value,
    body: &Value,
    (request, incoming): (Request<'_>, &BTreeMap<String, String>),
    observation: &mut RequestObservation,
    (compact, register): (
        bool,
        impl FnOnce(Caller, Option<String>, Option<CallLease>) -> Option<String>,
    ),
    monitor: &mut DisconnectMonitor,
) -> Result<Upstream, RelayError> {
    observation.aero_parent();
    observation.phase(Phase::ValidateInput);
    let anchor =
        emp_context::request_history_anchor(body.as_object().expect("request body"), incoming).ok();
    let session = anchor
        .as_ref()
        .and_then(|anchor| anchor.thread_id.as_deref())
        .or_else(|| {
            incoming
                .iter()
                .find(|(key, _)| {
                    key.eq_ignore_ascii_case("session-id")
                        || key.eq_ignore_ascii_case("session_id")
                        || key.eq_ignore_ascii_case("x-session-id")
                })
                .map(|(_, value)| value.as_str())
        })
        .or_else(|| body["prompt_cache_key"].as_str())
        .ok_or(RelayError::Request(
            400,
            "aero_session_required",
            "Aero requires a stable session-id header or prompt_cache_key.",
        ))?;
    observation.phase(Phase::Execute);
    if request
        .source
        .is_some_and(|source| source.delegation.is_some())
    {
        return Err(RelayError::Request(
            403,
            "aero_recursive_route",
            "An Aero role must use an EMP model source.",
        ));
    }
    let endpoint = match tokio::task::block_in_place(|| state.backend.modules.aero(state, config)) {
        Ok(endpoint) => endpoint,
        Err(error) if error == "module_disabled" => {
            return Err(RelayError::Request(
                404,
                "module_disabled",
                "Enable the Aero module to use this model.",
            ));
        }
        Err(_) => return Err(RelayError::unavailable()),
    };
    let caller = request
        .source
        .and_then(|source| source.caller.clone())
        .unwrap_or_else(|| crate::services::gateway_clients::Caller {
            id: "local".into(),
            name: String::new(),
        });
    let caller_id = caller.id.clone();
    let caller_key = register(
        caller,
        request.header("Authorization").map(str::to_owned),
        request.source.and_then(|source| source.aero_lease.clone()),
    )
    .ok_or_else(RelayError::unavailable)?;
    let mut headers = BTreeMap::from([
        (
            "Authorization".into(),
            format!("Bearer {}", endpoint.inbound_key),
        ),
        ("Content-Type".into(), "application/json".into()),
        ("session_id".into(), session.to_owned()),
        ("X-Aero-Caller-Key".into(), caller_key),
        ("X-Aero-Caller-Id".into(), caller_id),
    ]);
    for (key, value) in incoming {
        if matches!(
            key.to_ascii_lowercase().as_str(),
            "x-codex-turn-metadata"
                | "x-codex-thread-id"
                | "x-codex-parent-thread-id"
                | "originator"
                | "version"
        ) {
            headers.insert(key.clone(), value.clone());
        }
    }
    let url = format!(
        "{}/responses{}",
        endpoint.url,
        if compact { "/compact" } else { "" }
    );
    let sanitized;
    let body = if body.get("_aero_visible_input").is_some() {
        sanitized = {
            let mut clean = body.clone();
            clean
                .as_object_mut()
                .expect("request object")
                .remove("_aero_visible_input");
            clean
        };
        &sanitized
    } else {
        body
    };
    let mut response = open_module(state, monitor, &url, &headers, body).await?;
    if response.status() == 409 && response.header("x-aero-history") == Some("required") {
        observation.phase(Phase::PrepareHistory);
        let reader = emp_codex::history::CodexHomeHistoryReader::new(&state.backend.codex.home);
        let visible =
            tokio::task::block_in_place(|| emp_context::prepare(body, incoming, false, &reader))
                .map_err(RelayError::History)?;
        let mut materialized = body.clone();
        materialized["_aero_visible_input"] = visible["input"].clone();
        response = open_module(state, monitor, &url, &headers, &materialized).await?;
        observation.phase(Phase::Execute);
    }
    Ok(Upstream {
        response,
        _relay: endpoint,
    })
}

async fn open_module(
    state: &ServerState,
    monitor: &mut DisconnectMonitor,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: &Value,
) -> Result<HttpResponse, RelayError> {
    let future = state.backend.transport.client.open(
        HttpMethod::Post,
        url,
        headers.clone(),
        Some(serde_json::to_vec(body).expect("request JSON")),
        true,
    );
    match await_module(state, monitor, future).await {
        DisconnectRace::Ready(Ok(response)) => Ok(response),
        DisconnectRace::Ready(Err(_)) => Err(RelayError::unavailable()),
        DisconnectRace::Disconnected => Err(RelayError::Disconnected),
    }
}
