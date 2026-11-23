use crate::{
    api::responses::ResponsesRequestResult,
    app::ServerState,
    http::{
        request::Request,
        response::{SECURITY_HEADERS, json_error_response, status_text},
    },
    services::{
        disconnect::{DisconnectMonitor, DisconnectRace},
        modules::aero::{Call, Relay},
        observation::request::{Phase, RequestObservation},
    },
};
use emp_transport::{HttpMethod, HttpResponse, SseJsonParser};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write, net::TcpStream};

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

pub(super) struct Upstream<'a> {
    pub(super) response: HttpResponse,
    _caller: Call<'a>,
    _relay: Relay,
}

pub(super) fn await_module<F: std::future::Future>(
    state: &ServerState,
    monitor: &mut DisconnectMonitor,
    future: F,
) -> DisconnectRace<F::Output> {
    state.backend.transport.runtime.block_on(async {
        tokio::select! {
            biased;
            _ = state.wait_for_shutdown() => DisconnectRace::Disconnected,
            result = monitor.race(future) => result,
        }
    })
}

pub(crate) fn relay(
    stream: &mut TcpStream,
    state: &ServerState,
    config: &Value,
    body: &Value,
    (request, incoming): (Request<'_>, &BTreeMap<String, String>),
    observation: &mut RequestObservation,
    compact: bool,
) -> ResponsesRequestResult {
    let upstream = DisconnectMonitor::start(stream)
        .map_err(|_| RelayError::unavailable())
        .and_then(|mut monitor| {
            open(
                state,
                config,
                body,
                (request, incoming),
                observation,
                compact,
                &mut monitor,
            )
            .map(|upstream| (upstream, monitor))
        });
    let (mut upstream, mut monitor) = match upstream {
        Ok(upstream) => upstream,
        Err(RelayError::Disconnected) => return ResponsesRequestResult::Streamed,
        Err(RelayError::History(error)) => {
            return ResponsesRequestResult::Buffered(
                crate::api::history_response::history_http_error(&error, observation),
            );
        }
        Err(RelayError::Request(status, code, message)) => {
            return ResponsesRequestResult::Buffered(json_error_response(
                status,
                status_text(status),
                message,
                Some(code),
                &[],
            ));
        }
    };
    let status = upstream.response.status();
    let content_type = upstream
        .response
        .header("content-type")
        .unwrap_or("application/json")
        .to_owned();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nConnection: close\r\nCache-Control: no-cache\r\n",
        status_text(status)
    );
    if observation
        .written(
            stream
                .write_all(head.as_bytes())
                .and_then(|()| stream.write_all(SECURITY_HEADERS))
                .and_then(|()| stream.write_all(b"\r\n")),
            false,
        )
        .is_err()
    {
        return ResponsesRequestResult::Streamed;
    }
    let mut parser = SseJsonParser::new();
    loop {
        match await_module(state, &mut monitor, upstream.response.next_chunk()) {
            DisconnectRace::Ready(Ok(Some(chunk))) => {
                if observation
                    .written(stream.write_all(&chunk), false)
                    .is_err()
                {
                    break;
                }
                if content_type.starts_with("text/event-stream")
                    && let Ok(events) = parser.push(&chunk)
                {
                    for event in events {
                        let _ = observation
                            .event_written(&Value::Object(event), Ok::<(), std::io::Error>(()));
                    }
                }
            }
            DisconnectRace::Ready(Ok(None)) => {
                if !content_type.starts_with("text/event-stream") {
                    observation.http_response(&json!(status), Ok(()));
                }
                break;
            }
            _ => break,
        }
    }
    ResponsesRequestResult::Streamed
}

pub(super) fn open<'a>(
    state: &'a ServerState,
    config: &Value,
    body: &Value,
    (request, incoming): (Request<'_>, &BTreeMap<String, String>),
    observation: &mut RequestObservation,
    compact: bool,
    monitor: &mut DisconnectMonitor,
) -> Result<Upstream<'a>, RelayError> {
    observation.diagnostic_only("aero");
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
    let endpoint = match state.backend.modules.aero(state, config) {
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
    let call = state
        .backend
        .aero_callers
        .delegate(caller, request.header("Authorization").map(str::to_owned))
        .ok_or_else(RelayError::unavailable)?;
    let mut headers = BTreeMap::from([
        (
            "Authorization".into(),
            format!("Bearer {}", endpoint.inbound_key),
        ),
        ("Content-Type".into(), "application/json".into()),
        ("session_id".into(), session.to_owned()),
        ("X-Aero-Caller-Key".into(), call.key.clone()),
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
    let mut open = |body: &Value| {
        let future = state.backend.transport.client.open(
            HttpMethod::Post,
            &url,
            headers.clone(),
            Some(serde_json::to_vec(body).expect("request JSON")),
            true,
        );
        match await_module(state, monitor, future) {
            DisconnectRace::Ready(Ok(response)) => Ok(response),
            DisconnectRace::Ready(Err(_)) => Err(RelayError::unavailable()),
            DisconnectRace::Disconnected => Err(RelayError::Disconnected),
        }
    };
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
    let mut response = open(body)?;
    if response.status() == 409 && response.header("x-aero-history") == Some("required") {
        observation.phase(Phase::PrepareHistory);
        let reader =
            emp_codex::history::CodexHomeHistoryReader::new(&state.backend.accounts.codex_home);
        let visible =
            emp_context::prepare(body, incoming, false, &reader).map_err(RelayError::History)?;
        let mut materialized = body.clone();
        materialized["_aero_visible_input"] = visible["input"].clone();
        response = open(&materialized)?;
        observation.phase(Phase::Execute);
    }
    Ok(Upstream {
        response,
        _caller: call,
        _relay: endpoint,
    })
}
