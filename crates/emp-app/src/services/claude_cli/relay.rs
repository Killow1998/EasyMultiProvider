use super::cancellation::Cancellation;
#[path = "relay_io.rs"]
mod io;
use io::{accept, read_accepted_request, reject_request, write_bytes, write_upstream_response};

use super::media::ExpectedUserContent;
use crate::app::ServerState;
use crate::services::claude_cli::ClaudeCliError;
use emp_core::ResolvedRoute;
use emp_router::ExternalRouter;
use serde_json::Value;
use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};

use std::sync::mpsc;

const MAX_RELAY_PROBE_REQUESTS: usize = 1;

pub(super) struct RelayResult {
    pub(super) status: u16,
    pub(super) observation: emp_router::model_observation::ModelObservation,
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run_once(
    listener: TcpListener,
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    expected_user_content: &ExpectedUserContent,
    token: &str,
    cancelled: &Cancellation,
    result_tx: mpsc::SyncSender<Result<RelayResult, ClaudeCliError>>,
) {
    let mut probe_requests = 0;
    let result = loop {
        let accepted = accept(&listener, cancelled);
        let accepted = match accepted {
            Ok(accepted) => accepted,
            Err(ClaudeCliError::Disconnected) => return,
            Err(error) => break Err(error),
        };
        match handle_request(
            accepted,
            state,
            route,
            incoming,
            expected_user_content,
            token,
            cancelled,
        ) {
            Ok(Some(result)) => break Ok(result),
            Ok(None) if probe_requests < MAX_RELAY_PROBE_REQUESTS => {
                probe_requests += 1;
            }
            Ok(None) => break Err(ClaudeCliError::Failure("claude_cli_relay_probe_limit")),
            Err(error) => break Err(error),
        }
    };
    if result.is_err() {
        // A terminal provider or local relay failure must stop the CLI's
        // retry loop now; its HTTP socket has no useful response to wait for.
        cancelled.cancel();
    }
    let _ = result_tx.send(result);
}

fn handle_request(
    mut stream: TcpStream,
    state: &ServerState,
    route: &ResolvedRoute,
    incoming: &BTreeMap<String, String>,
    expected_user_content: &ExpectedUserContent,
    token: &str,
    cancelled: &Cancellation,
) -> Result<Option<RelayResult>, ClaudeCliError> {
    let mut request = read_accepted_request(&mut stream, token, cancelled)?;
    let health_preflight = request.method == "HEAD" && request.path == "/api/hello";
    if health_preflight {
        write_bytes(
            &mut stream,
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            cancelled,
        )?;
        return Ok(None);
    }
    if let Err(code) = transcript::validate(&mut request.body, expected_user_content) {
        reject_request(&mut stream, 400, "invalid_request", cancelled);
        return Err(ClaudeCliError::Failure(code));
    }
    let router = ExternalRouter::new(&state.backend.transport.client);
    let request_body = &request.body;
    let protocol_headers = &request.protocol_headers;
    let routed = state.backend.transport.runtime.block_on(async {
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => None,
            result = router.execute_anthropic_passthrough(
                route,
                request_body,
                incoming,
                protocol_headers,
            ) => Some(result),
        }
    });
    let Some(routed) = routed else {
        return Err(ClaudeCliError::Disconnected);
    };
    let routed = routed.map_err(ClaudeCliError::Router)?;
    if (200..300).contains(&routed.status) && super::output_budget::exhausted(&routed.body) {
        return Err(ClaudeCliError::Failure(super::output_budget::ERROR));
    }
    write_upstream_response(&mut stream, &routed, cancelled)?;
    let mut models = emp_router::model_observation::ModelObservation::default();
    if let Ok(value) = serde_json::from_slice::<Value>(&routed.body) {
        models.observe(&value, false);
    } else {
        for line in routed.body.split(|byte| *byte == b'\n') {
            if let Some(data) = line
                .strip_prefix(b"data: ")
                .or_else(|| line.strip_prefix(b"data:"))
                && let Ok(value) = serde_json::from_slice::<Value>(data)
            {
                models.observe(&value, false);
            }
        }
    }
    Ok(Some(RelayResult {
        observation: models,
        status: routed.status,
    }))
}

#[cfg(test)]
mod tests;
mod transcript;
