//! One-request Claude Code adapter. EMP owns history, tools, routing, and
//! response projection; Claude Code performs a single inference through the
//! request-local Anthropic Messages relay.

#[path = "claude_cli/auth.rs"]
mod auth;
#[path = "claude_cli/image_geometry.rs"]
mod image_geometry;
#[path = "claude_cli/media.rs"]
mod media;
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
        "claude_cli_local_home_unavailable" => (
            503,
            "claude_cli_local_home_unavailable",
            "Claude Code local sign-in is unavailable to EMP; sign in using the same operating-system account that runs EMP",
        ),
        "claude_cli_login_required" => (
            401,
            "claude_cli_login_required",
            "Claude Code is not signed in to a Claude subscription; sign in to Claude Code manually and retry",
        ),
        "claude_cli_subscription_required" => (
            403,
            "claude_cli_subscription_required",
            "The current Claude Code login is not a Claude subscription login; sign in to the intended Claude subscription and retry",
        ),
        "claude_cli_auth_status_unknown" | "claude_cli_auth_status_unavailable" => (
            503,
            "claude_cli_auth_status_unavailable",
            "EMP could not verify Claude Code subscription sign-in; check Claude Code sign-in and retry",
        ),
        "claude_cli_auth_status_timeout" => (
            504,
            "claude_cli_auth_status_timeout",
            "Claude Code sign-in status check timed out; try again",
        ),
        "claude_cli_local_credentials_present" => (
            400,
            "claude_cli_local_credentials_present",
            "Claude Code local login cannot include provider URL or API key credentials",
        ),
        "claude_cli_local_request_failed" => (
            502,
            "claude_cli_local_request_failed",
            "Claude Code could not complete this local subscription request; check Claude Code sign-in and retry",
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
            "Claude Code CLI cannot safely resolve this image, audio, video, or document input",
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
        "claude_cli_transcript_mismatch" => (
            502,
            "claude_cli_transcript_mismatch",
            "Claude Code changed the conversation format; EMP could not forward the request",
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

pub(crate) fn preflight_input(body: &Value) -> Result<(), ClaudeCliError> {
    media::preflight(body).map_err(ClaudeCliError::Failure)
}

pub(crate) fn context_estimation_payload(body: &Value) -> Result<Value, ClaudeCliError> {
    media::context_payload(body).map_err(ClaudeCliError::Failure)
}

pub(crate) fn execute_complete(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    monitor: Option<&mut DisconnectMonitor>,
) -> Result<ClaudeCliResult, ClaudeCliError> {
    let started = Instant::now();
    let result = (|| {
        if state.shutdown.load(Ordering::Acquire) {
            return Err(ClaudeCliError::ShuttingDown);
        }
        preflight_input(body)?;
        let executable = emp_codex::installed_cli::resolve_claude_cli()
            .ok_or(ClaudeCliError::Failure("claude_cli_unavailable"))?;
        crate::services::observation::request_started(state, route, body, incoming);
        execute_complete_with_cli(state, route, body, incoming, ids, &executable, monitor)
    })();
    if let Err(error) = &result {
        if matches!(error, ClaudeCliError::Disconnected) {
            crate::services::observation::request_cancelled(state, route, incoming);
        }
        let (status, code) = match error {
            ClaudeCliError::Failure(code) => (failure_details(code).0, *code),
            ClaudeCliError::Router(error) => (error.status(), error.error_class().as_str()),
            ClaudeCliError::Disconnected => (499, "client_disconnected"),
            ClaudeCliError::ShuttingDown => (503, "server_shutting_down"),
        };
        state.backend.diagnostics.journal.event(
            "warning",
            "claude_cli_request_failed",
            &json!({"status":status,"error_code":code,
                "duration_ms":started.elapsed().as_millis() as u64}),
        );
    }
    result
}

fn execute_complete_with_cli(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    executable: &emp_codex::installed_cli::InstalledClaudeCli,
    mut monitor: Option<&mut DisconnectMonitor>,
) -> Result<ClaudeCliResult, ClaudeCliError> {
    let protocol = protocol_candidates(route)
        .into_iter()
        .find(|protocol| *protocol == Protocol::AnthropicMessages)
        .ok_or(ClaudeCliError::Failure("unsupported_provider_protocol"))?;
    let candidate = route
        .with_protocol(protocol)
        .map_err(|_| ClaudeCliError::Failure("unsupported_provider_protocol"))?;
    let local_login = candidate
        .provider
        .value()
        .get("auth_mode")
        .and_then(Value::as_str)
        == Some("claude_login");
    if local_login
        && ["base_url", "api_key", "api_key_file"]
            .into_iter()
            .any(|field| {
                candidate
                    .provider
                    .value()
                    .get(field)
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
            })
    {
        return Err(ClaudeCliError::Failure(
            "claude_cli_local_credentials_present",
        ));
    }
    let cli_input = media::prepare(body).map_err(ClaudeCliError::Failure)?;
    let stdin = Arc::<[u8]>::from(cli_input.stdin);
    let expected_user_content = cli_input.expected_user_content;
    let model = candidate.upstream_model.as_str();
    let effort = projection::effort(body).map_err(ClaudeCliError::Failure)?;
    let schema = projection::proposal_schema().map_err(ClaudeCliError::Failure)?;

    let temp =
        TempDir::new().map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let isolated_home = temp.path().join("home");
    let config_home = temp.path().join("xdg-config");
    let cache_home = temp.path().join("xdg-cache");
    let working_directory = temp.path().join("workspace");
    std::fs::create_dir_all(&config_home)
        .and_then(|()| std::fs::create_dir_all(&cache_home))
        .and_then(|()| std::fs::create_dir_all(&working_directory))
        .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    if !local_login {
        std::fs::create_dir_all(&isolated_home)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    }
    let local_home = if local_login {
        Some(
            process::host_cli_home()
                .filter(|home| home.is_absolute() && home.is_dir())
                .ok_or(ClaudeCliError::Failure("claude_cli_local_home_unavailable"))?,
        )
    } else {
        None
    };
    let prompt_file = temp.path().join("system-prompt.txt");
    std::fs::write(&prompt_file, projection::SYSTEM_PROMPT)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let request_started = Instant::now();
    let (stdout, provider_status) = if local_login {
        let local_home = local_home.as_deref().expect("checked local CLI home");
        verify_local_subscription(
            state,
            executable,
            local_home,
            &config_home,
            &cache_home,
            temp.path(),
            &working_directory,
            &cancelled,
            &mut monitor,
        )?;
        let command = process::command(process::CommandConfig {
            executable: &executable.executable,
            child_path: &executable.child_path,
            model,
            effort,
            prompt_file: &prompt_file,
            schema: &schema,
            input_format: cli_input.input_format,
            mode: process::InvocationMode::LocalLogin { home: local_home },
            config_home: &config_home,
            cache_home: &cache_home,
            temp: temp.path(),
            working_directory: &working_directory,
        });
        let output = process::run(command, Arc::clone(&stdin), &cancelled, || {
            cancellation_reason(state, &mut monitor)
        })
        .map_err(|code| local_or_process_error(code, true))?;
        (output, 200)
    } else {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
        listener
            .set_nonblocking(true)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?;
        let port = listener
            .local_addr()
            .map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?
            .port();
        let token = crate::util::random_hex(32)
            .map_err(|_| ClaudeCliError::Failure("claude_cli_token_unavailable"))?;
        let command = process::command(process::CommandConfig {
            executable: &executable.executable,
            child_path: &executable.child_path,
            model,
            effort,
            prompt_file: &prompt_file,
            schema: &schema,
            input_format: cli_input.input_format,
            mode: process::InvocationMode::CpaRelay {
                port,
                token: &token,
                home: &isolated_home,
            },
            config_home: &config_home,
            cache_home: &cache_home,
            temp: temp.path(),
            working_directory: &working_directory,
        });
        let (relay_result_tx, relay_result_rx) = mpsc::sync_channel(1);
        let process_result = thread::scope(|scope| {
            let relay_cancelled = Arc::clone(&cancelled);
            let relay_result_tx = relay_result_tx.clone();
            let relay_expected_content = &expected_user_content;
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
                    relay_expected_content,
                    relay_token,
                    &relay_cancelled,
                    relay_result_tx,
                );
            });
            let process_result = process::run(command, Arc::clone(&stdin), &cancelled, || {
                cancellation_reason(state, &mut monitor)
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
        let output = process_result.map_err(|code| local_or_process_error(code, false))?;
        let relay_result =
            relay_result.ok_or(ClaudeCliError::Failure("claude_cli_no_provider_response"))??;
        (output, relay_result.status)
    };
    let cli_output = projection::parse_cli_result(&stdout).map_err(|code| {
        if local_login && code == "claude_cli_result_error" {
            ClaudeCliError::Failure("claude_cli_local_request_failed")
        } else {
            ClaudeCliError::Failure(code)
        }
    })?;
    let response =
        projection::response_from_cli(&cli_output, &candidate, body, ids, provider_status)?;
    Ok(ClaudeCliResult::Completed(ClaudeCliCompletion {
        response,
        route: candidate,
        request_started,
    }))
}

fn cancellation_reason(
    state: &ServerState,
    monitor: &mut Option<&mut DisconnectMonitor>,
) -> Option<process::CancellationReason> {
    if state.shutdown.load(Ordering::Acquire) {
        return Some(process::CancellationReason::ServerShutdown);
    }
    monitor.as_deref_mut().and_then(|monitor| {
        matches!(
            state
                .backend
                .transport
                .runtime
                .block_on(async { monitor.race(tokio::time::sleep(CHILD_POLL)).await }),
            DisconnectRace::Disconnected
        )
        .then_some(process::CancellationReason::DownstreamDisconnected)
    })
}

fn local_or_process_error(code: &'static str, local_login: bool) -> ClaudeCliError {
    match code {
        "claude_cli_disconnected" => ClaudeCliError::Disconnected,
        "claude_cli_shutdown" => ClaudeCliError::ShuttingDown,
        "claude_cli_process_failed" if local_login => {
            ClaudeCliError::Failure("claude_cli_local_request_failed")
        }
        code => ClaudeCliError::Failure(code),
    }
}

#[allow(clippy::too_many_arguments)]
fn verify_local_subscription(
    state: &ServerState,
    executable: &emp_codex::installed_cli::InstalledClaudeCli,
    home: &std::path::Path,
    config_home: &std::path::Path,
    cache_home: &std::path::Path,
    temp: &std::path::Path,
    working_directory: &std::path::Path,
    cancelled: &AtomicBool,
    monitor: &mut Option<&mut DisconnectMonitor>,
) -> Result<(), ClaudeCliError> {
    let command = process::auth_status_command(process::AuthStatusCommandConfig {
        executable: &executable.executable,
        child_path: &executable.child_path,
        home,
        config_home,
        cache_home,
        temp,
        working_directory,
    });
    let stdout =
        process::run_auth_status(command, cancelled, || cancellation_reason(state, monitor))
            .map_err(|code| match code {
                "claude_cli_disconnected" => ClaudeCliError::Disconnected,
                "claude_cli_shutdown" => ClaudeCliError::ShuttingDown,
                "claude_cli_timeout" => ClaudeCliError::Failure("claude_cli_auth_status_timeout"),
                _ => ClaudeCliError::Failure("claude_cli_auth_status_unavailable"),
            })?;
    match auth::parse_status(&stdout) {
        auth::LoginStatus::SubscriptionOAuth => Ok(()),
        auth::LoginStatus::Missing => Err(ClaudeCliError::Failure("claude_cli_login_required")),
        auth::LoginStatus::ApiKey => {
            Err(ClaudeCliError::Failure("claude_cli_subscription_required"))
        }
        auth::LoginStatus::Unknown => {
            Err(ClaudeCliError::Failure("claude_cli_auth_status_unknown"))
        }
    }
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
