//! One-request Claude Code adapter. EMP owns history, tools, routing, and
//! response projection; Claude Code performs a single inference through the
//! request-local Anthropic Messages relay.

#[path = "claude_cli/process.rs"]
mod process;
#[path = "claude_cli/projection.rs"]
mod projection;
#[path = "claude_cli/relay.rs"]
mod relay;

use crate::app::ServerState;
use crate::http::response::{json_error_response, status_text};
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace};
use emp_core::{Protocol, ResolvedRoute};
use emp_router::{CompleteResponse, ProjectionIds, RouterError, protocol_candidates};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const CHILD_POLL: Duration = Duration::from_millis(25);

#[derive(Debug)]
pub(crate) enum ClaudeCliError {
    Disconnected,
    ShuttingDown,
    Router(RouterError),
    Failure(&'static str),
}

fn failure_details(code: &'static str) -> (u16, &'static str, &'static str) {
    match code {
        "claude_cli_unavailable" => (
            503,
            "claude_cli_unavailable",
            "Claude Code CLI is unavailable or not trusted",
        ),
        "claude_cli_timeout" => (
            504,
            "claude_cli_timeout",
            "Claude Code CLI request timed out",
        ),
        "unsupported_reasoning_effort" => (
            400,
            "unsupported_reasoning_effort",
            "reasoning effort is not supported by Claude Code CLI",
        ),
        "unsupported_input_modality" => (
            400,
            "unsupported_input_modality",
            "Claude Code CLI currently supports text-only Responses input",
        ),
        "claude_cli_input_too_large" | "claude_cli_schema_too_large" => (
            413,
            "claude_cli_input_too_large",
            "Claude Code CLI request exceeds its size limit",
        ),
        "unsupported_provider_protocol" => (
            501,
            "unsupported_provider_protocol",
            "Claude Code CLI requires an Anthropic Messages provider route",
        ),
        "claude_cli_invalid_tool_history" => (
            422,
            "claude_cli_invalid_tool_history",
            "Responses tool history is invalid for this request",
        ),
        "claude_cli_unknown_tool_proposal" => (
            502,
            "claude_cli_unknown_tool_proposal",
            "Claude Code proposed a tool that is not available in this request",
        ),
        _ => (502, code, "Claude Code CLI request failed"),
    }
}

impl ClaudeCliError {
    pub(crate) fn http_response(&self) -> Vec<u8> {
        match self {
            Self::Disconnected => Vec::new(),
            Self::ShuttingDown => json_error_response(
                503,
                status_text(503),
                "EMP is shutting down",
                Some("server_shutting_down"),
                &[],
            ),
            Self::Router(error) => crate::services::failures::router_error_response(error.clone()),
            Self::Failure(code) => {
                let (status, failure_code, message) = failure_details(code);
                json_error_response(
                    status,
                    status_text(status),
                    message,
                    Some(failure_code),
                    &[],
                )
            }
        }
    }

    pub(crate) fn websocket_value(&self) -> Value {
        match self {
            Self::Disconnected => {
                json!({"type":"error","status":499,"error":{"code":"client_disconnected","message":"client disconnected"}})
            }
            Self::ShuttingDown => {
                json!({"type":"error","status":503,"error":{"code":"server_shutting_down","message":"EMP is shutting down"}})
            }
            Self::Router(error) => crate::services::failures::websocket_router_error(error),
            Self::Failure(code) => {
                let (status, failure_code, message) = failure_details(code);
                json!({"type":"error","status":status,"error":{"code":failure_code,"message":message}})
            }
        }
    }
}

pub(crate) struct ClaudeCliCompletion {
    pub(crate) response: CompleteResponse,
    pub(crate) route: ResolvedRoute,
    pub(crate) request_started: Instant,
}

pub(crate) enum ClaudeCliResult {
    Completed(ClaudeCliCompletion),
}

pub(crate) fn selected(route: &ResolvedRoute) -> bool {
    route
        .provider
        .value()
        .get("execution_backend")
        .and_then(Value::as_str)
        == Some("claude_cli")
}

pub(crate) fn execute_complete(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    mut monitor: Option<&mut DisconnectMonitor>,
) -> Result<ClaudeCliResult, ClaudeCliError> {
    if state.shutdown.load(Ordering::Acquire) {
        return Err(ClaudeCliError::ShuttingDown);
    }
    let executable = emp_codex::installed_cli::resolve_claude_cli()
        .ok_or(ClaudeCliError::Failure("claude_cli_unavailable"))?;
    let protocol = protocol_candidates(route)
        .into_iter()
        .find(|protocol| *protocol == Protocol::AnthropicMessages)
        .ok_or(ClaudeCliError::Failure("unsupported_provider_protocol"))?;
    let candidate = route
        .with_protocol(protocol)
        .map_err(|_| ClaudeCliError::Failure("unsupported_provider_protocol"))?;
    let transcript =
        Arc::<[u8]>::from(projection::transcript(body).map_err(ClaudeCliError::Failure)?);
    let model = candidate.upstream_model.as_str();
    let effort = projection::effort(body).map_err(ClaudeCliError::Failure)?;
    let schema = projection::proposal_schema().map_err(ClaudeCliError::Failure)?;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
    listener
        .set_nonblocking(true)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
    let port = listener
        .local_addr()
        .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?
        .port();
    let temp =
        TempDir::new().map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let home = temp.path().join("home");
    let config_home = home.join(".config");
    let cache_home = home.join(".cache");
    std::fs::create_dir_all(&config_home)
        .and_then(|()| std::fs::create_dir_all(&cache_home))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let prompt_file = temp.path().join("system-prompt.txt");
    std::fs::write(&prompt_file, projection::SYSTEM_PROMPT)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let token = crate::util::random_hex(32)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_token_unavailable"))?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let request_started = Instant::now();
    let (relay_result_tx, relay_result_rx) = mpsc::sync_channel(1);
    let command = process::command(process::CommandConfig {
        executable: &executable.executable,
        child_path: &executable.child_path,
        model,
        effort,
        prompt_file: &prompt_file,
        schema: &schema,
        port,
        token: &token,
        home: &home,
        config_home: &config_home,
        cache_home: &cache_home,
        temp: temp.path(),
    });

    let process_output = thread::scope(|scope| {
        let relay_cancelled = Arc::clone(&cancelled);
        let relay_result_tx = relay_result_tx.clone();
        let relay_transcript = Arc::clone(&transcript);
        let relay_state = state;
        let relay_route = &candidate;
        let relay_incoming = incoming;
        let relay_token = &token;
        let relay = scope.spawn(move || {
            relay::run_once(
                listener,
                relay_state,
                relay_route,
                relay_incoming,
                &relay_transcript,
                relay_token,
                &relay_cancelled,
                relay_result_tx,
            );
        });
        let process_result = process::run(command, Arc::clone(&transcript), &cancelled, || {
            if state.shutdown.load(Ordering::Acquire) {
                return Some(process::CancellationReason::ServerShutdown);
            }
            if let Some(monitor) = monitor.as_deref_mut() {
                matches!(
                    state
                        .backend
                        .transport
                        .runtime
                        .block_on(async { monitor.race(tokio::time::sleep(CHILD_POLL)).await }),
                    DisconnectRace::Disconnected
                )
                .then_some(process::CancellationReason::DownstreamDisconnected)
            } else {
                None
            }
        });
        if !matches!(
            process_result,
            Err("claude_cli_disconnected" | "claude_cli_shutdown")
        ) {
            cancelled.store(true, Ordering::Release);
        }
        let _ = relay.join();
        process_result
    });
    let relay_result = relay_result_rx.try_recv().ok();
    if let Some(Err(error)) = &relay_result {
        match error {
            ClaudeCliError::Router(error) => {
                return Err(ClaudeCliError::Router(error.clone()));
            }
            ClaudeCliError::Failure(code) => return Err(ClaudeCliError::Failure(code)),
            ClaudeCliError::ShuttingDown => return Err(ClaudeCliError::ShuttingDown),
            ClaudeCliError::Disconnected => {}
        }
    }
    let stdout = match process_output {
        Ok(stdout) => stdout,
        Err("claude_cli_disconnected") => return Err(ClaudeCliError::Disconnected),
        Err("claude_cli_shutdown") => return Err(ClaudeCliError::ShuttingDown),
        Err(code) => {
            return Err(ClaudeCliError::Failure(code));
        }
    };
    let relay_result =
        relay_result.ok_or(ClaudeCliError::Failure("claude_cli_no_provider_response"))??;
    let cli_output = projection::parse_cli_result(&stdout).map_err(ClaudeCliError::Failure)?;
    let response =
        projection::response_from_cli(&cli_output, &candidate, body, ids, relay_result.status)?;
    Ok(ClaudeCliResult::Completed(ClaudeCliCompletion {
        response,
        route: candidate,
        request_started,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_cli_failure_status_and_code_match_http_and_websocket() {
        for (reason, expected_status, expected_code) in [
            (
                "claude_cli_input_too_large",
                413,
                "claude_cli_input_too_large",
            ),
            (
                "claude_cli_schema_too_large",
                413,
                "claude_cli_input_too_large",
            ),
            (
                "claude_cli_invalid_tool_history",
                422,
                "claude_cli_invalid_tool_history",
            ),
        ] {
            let error = ClaudeCliError::Failure(reason);
            let http = String::from_utf8(error.http_response()).expect("HTTP response UTF-8");
            let (head, body) = http.split_once("\r\n\r\n").expect("HTTP response body");
            assert!(head.starts_with(&format!("HTTP/1.1 {expected_status} ")));
            let http_value: Value = serde_json::from_str(body).expect("HTTP error JSON");
            assert_eq!(http_value["error"]["code"], expected_code);

            let websocket_value = error.websocket_value();
            assert_eq!(websocket_value["status"], expected_status);
            assert_eq!(websocket_value["error"]["code"], expected_code);
        }
    }
}
