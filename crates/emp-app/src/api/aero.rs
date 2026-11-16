use crate::{
    api::responses::ResponsesRequestResult,
    app::ServerState,
    http::response::{SECURITY_HEADERS, json_error_response, status_text},
    services::{
        disconnect::{DisconnectMonitor, DisconnectRace},
        observation::request::{Phase, RequestObservation},
    },
};
use emp_transport::{HttpMethod, SseJsonParser};
use serde_json::{Value, json};
use std::{collections::BTreeMap, io::Write, net::TcpStream};

pub(crate) fn relay(
    stream: &mut TcpStream,
    state: &ServerState,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    observation: &mut RequestObservation,
) -> ResponsesRequestResult {
    observation.diagnostic_only("aero");
    observation.phase(Phase::ValidateInput);
    let fail = |status, message: &str| {
        let message = match message {
            "aero.runtime_missing" => {
                "aero runtime is missing. Use the complete experimental package, including its aero folder."
            }
            "aero.runtime_unavailable" => {
                "aero could not start or connect. Check the experimental runtime package."
            }
            "aero.session_required" => {
                "aero requires a stable session-id header or prompt_cache_key."
            }
            other => other,
        };
        ResponsesRequestResult::Buffered(json_error_response(
            status,
            status_text(status),
            message,
            None,
            &[],
        ))
    };
    let anchor =
        emp_history::request_history_anchor(body.as_object().expect("request body"), incoming).ok();
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
        .or_else(|| body["prompt_cache_key"].as_str());
    let Some(session) = session else {
        return fail(400, "aero.session_required");
    };
    observation.phase(Phase::Execute);
    let endpoint = match state.backend.aero.endpoint(state, config) {
        Ok(endpoint) => endpoint,
        Err(error) => return fail(503, &error),
    };
    let mut headers = BTreeMap::from([
        (
            "Authorization".into(),
            format!("Bearer {}", state.backend.gateway_key),
        ),
        ("Content-Type".into(), "application/json".into()),
        ("session_id".into(), session.to_owned()),
    ]);
    if let Some((_, value)) = incoming
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("authorization"))
    {
        headers.insert(
            "X-Aero-Caller-Key".into(),
            value.strip_prefix("Bearer ").unwrap_or(value).to_owned(),
        );
    }
    let Ok(mut monitor) = DisconnectMonitor::start(stream) else {
        return fail(503, "aero.runtime_unavailable");
    };
    let runtime = &state.backend.transport.runtime;
    let url = format!("{endpoint}/responses");
    let opened = runtime.block_on(async {
        tokio::select! {
            result = monitor.race(state.backend.transport.client.open(HttpMethod::Post,&url,headers,Some(serde_json::to_vec(body).expect("request JSON")),true)) => result,
            _ = state.wait_for_shutdown() => DisconnectRace::Disconnected,
        }
    });
    let mut upstream = match opened {
        DisconnectRace::Ready(Ok(response)) => response,
        DisconnectRace::Ready(Err(_)) => return fail(503, "aero.runtime_unavailable"),
        DisconnectRace::Disconnected => return ResponsesRequestResult::Streamed,
    };
    let status = upstream.status();
    let content_type = upstream
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
        let next = runtime.block_on(async {
            tokio::select! {
                result = monitor.race(upstream.next_chunk()) => result,
                _ = state.wait_for_shutdown() => DisconnectRace::Disconnected,
            }
        });
        match next {
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
