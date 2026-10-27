//! Establish large native context using generation-free, connection-local turns.
//! Never split an input item, break a tool pair, or replay a generated turn.
use super::super::ObservedWebSocket;
use super::super::duplex::{Duplex, Message};
use super::errors::native_stream_error_value_for_route;
use crate::services::observation::continuation::ContinuationReason;
use crate::services::request_outcome::RequestOutcome;
use emp_core::ResolvedRoute;
use emp_transport::{ClientWebSocket, FailureClass};
use serde_json::Value;

mod plan;
pub(super) use plan::Prefill;
use plan::{MESSAGE_BUDGET, json_bytes};

pub(super) enum PrefillError {
    Disconnected,
    Transport(std::io::Error),
    Rejected(Value),
    InvalidResponse,
}

impl Prefill<'_> {
    /// Prefix completions stay private. Only the final generated turn is
    /// projected to the client and counted as inference/usage by its owner.
    pub(super) fn run(
        self,
        client: &mut ClientWebSocket,
        duplex: &mut Duplex,
        websocket: &mut ObservedWebSocket<'_, '_>,
        pending_request: &mut Option<String>,
    ) -> Result<Value, PrefillError> {
        let count = self.chunks.len();
        let mut previous = None;
        for (index, chunk) in self.chunks.into_iter().enumerate() {
            let mut payload = self.base.clone();
            payload.insert("input".into(), Value::Array(chunk.to_vec()));
            if let Some(id) = previous.take() {
                payload.insert("previous_response_id".into(), id);
            }
            if index + 1 == count {
                return Ok(Value::Object(payload));
            }
            payload.insert("generate".into(), Value::Bool(false));
            let payload = Value::Object(payload);
            let bytes = json_bytes(&payload);
            if bytes > MESSAGE_BUDGET {
                return Err(PrefillError::InvalidResponse);
            }
            client
                .send_json(&payload)
                .map_err(|error| PrefillError::Transport(std::io::Error::other(error)))?;
            websocket.observation.native_prefill_sent(bytes);
            loop {
                let message = duplex
                    .next(client, websocket.inner, pending_request.is_none())
                    .map_err(PrefillError::Transport)?
                    .ok_or_else(|| {
                        PrefillError::Transport(std::io::ErrorKind::UnexpectedEof.into())
                    })?;
                let event = match message {
                    Message::Upstream(event) => event,
                    Message::DownstreamClosed => return Err(PrefillError::Disconnected),
                    Message::Downstream(text) => {
                        if serde_json::from_str::<Value>(&text)
                            .ok()
                            .is_some_and(|value| value["type"] == "response.interrupt")
                        {
                            client.send_text(&text).map_err(|error| {
                                PrefillError::Transport(std::io::Error::other(error))
                            })?;
                        } else {
                            *pending_request = Some(text);
                        }
                        continue;
                    }
                };
                let kind = event["type"].as_str().unwrap_or("");
                // A peer that generates during prefill must never cause an
                // automatic replay, a second tool execution, or false success.
                if kind.starts_with("response.output")
                    || kind.starts_with("response.function")
                    || event["response"]["output"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty())
                    || event["response"]["usage"]["output_tokens"]
                        .as_u64()
                        .is_some_and(|tokens| tokens > 0)
                {
                    return Err(PrefillError::InvalidResponse);
                }
                match kind {
                    "response.completed" => {
                        if event["response"]["status"] != "completed"
                            || !event["response"]["output"]
                                .as_array()
                                .is_some_and(Vec::is_empty)
                        {
                            return Err(PrefillError::InvalidResponse);
                        }
                        let id = event["response"]["id"]
                            .as_str()
                            .filter(|id| !id.is_empty() && id.len() <= 256)
                            .ok_or(PrefillError::InvalidResponse)?;
                        previous = Some(Value::String(id.to_owned()));
                        break;
                    }
                    "response.failed" | "response.incomplete" | "error" => {
                        return Err(PrefillError::Rejected(event));
                    }
                    _ => {}
                }
            }
        }
        unreachable!("a prefill plan has at least two chunks")
    }
}

impl PrefillError {
    pub(super) fn report(
        self,
        route: &ResolvedRoute,
        websocket: &mut ObservedWebSocket<'_, '_>,
        usage: &mut RequestOutcome<'_>,
    ) {
        if let Self::Transport(error) = &self {
            websocket.observation.native_continuation().transport_error = Some(error.to_string());
        }
        websocket.observation.native_continuation().reason = Some(match &self {
            Self::Rejected(event)
                if event["error"]["code"] == "previous_response_not_found"
                    || event["response"]["error"]["code"] == "previous_response_not_found" =>
            {
                ContinuationReason::UpstreamRecoveryRequired
            }
            Self::Rejected(_) => ContinuationReason::PrefillRejected,
            Self::Transport(_) => ContinuationReason::UpstreamDisconnected,
            _ => ContinuationReason::PrefillInvalidResponse,
        });
        let upstream_rejection = matches!(&self, Self::Rejected(_));
        let event = match self {
            Self::Rejected(event) => event,
            Self::Transport(_) => native_stream_error_value_for_route(
                502,
                FailureClass::Network,
                Some("native_prefill_failed"),
                "",
                route,
                false,
            ),
            _ => native_stream_error_value_for_route(
                502,
                FailureClass::StreamIncomplete,
                Some("native_prefill_invalid"),
                "",
                route,
                false,
            ),
        };
        if upstream_rejection {
            usage.observe_upstream(&event);
            let _ = websocket.send_upstream_json(&event);
        } else {
            usage.observe(&event);
            let _ = websocket.send_json(&event);
        }
    }
}
