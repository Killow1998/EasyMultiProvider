//! One-request Claude Code adapter. EMP owns history, tools, routing, and
//! response projection; Claude Code performs a single inference through the
//! request-local Anthropic Messages relay.

mod auth;
mod cancellation;
mod local_query;
pub(crate) mod model_query;
pub(crate) mod quota;
pub(crate) mod quota_query;
use cancellation::Cancellation;
mod image_geometry;
mod instructions;
#[cfg(test)]
mod instructions_tests;
mod media;
mod output_budget;
mod process;
mod projection;
mod relay;

use crate::app::ServerState;
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace};
use emp_core::{Protocol, ResolvedRoute};
use emp_router::{CompleteResponse, protocol_candidates};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod error;
pub(crate) use error::{ClaudeCliError, failure_details, failure_stage};

const CHILD_POLL: Duration = Duration::from_millis(25);

pub(crate) struct ClaudeCliCompletion {
    pub(crate) response: CompleteResponse,
    pub(crate) route: ResolvedRoute,
    pub(crate) request_started: Instant,
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

fn configured_limit(route: &ResolvedRoute, field: &str) -> Option<u64> {
    [route.model.value(), route.provider.value()]
        .into_iter()
        .find_map(|record| record.get(field)?.as_u64().filter(|value| *value > 0))
}

pub(crate) fn execute_complete(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    monitor: Option<&mut DisconnectMonitor>,
) -> Result<ClaudeCliCompletion, ClaudeCliError> {
    let started = Instant::now();
    let result = (|| {
        if state.shutdown.load(Ordering::Acquire) {
            return Err(ClaudeCliError::ShuttingDown);
        }
        preflight_input(body)?;
        let executable = emp_codex::installed_cli::resolve_claude_cli()
            .ok_or(ClaudeCliError::Failure("claude_cli_unavailable"))?;
        crate::services::observation::execution_attempt(state, route, body, incoming);
        execute_complete_with_cli(state, route, body, incoming, &executable, monitor)
    })();
    if let Err(error) = &result {
        if matches!(error, ClaudeCliError::Failure("claude_cli_interrupted")) {
            crate::services::observation::request_interrupted(state, route, incoming);
        } else if matches!(error, ClaudeCliError::Disconnected) {
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
            &json!({"request_id":crate::services::observation::request::request_id(incoming),"status":status,"error_code":code,
                "stage":failure_stage(error), "error_origin":crate::services::error_origin::claude(error),
                "provider_id":route.provider_id,
                "model_id":route.requested_model,
                "upstream_model":route.upstream_model,
                "reasoning_effort":projection::effort_diagnostic(body),
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
    executable: &emp_codex::installed_cli::InstalledClaudeCli,
    mut monitor: Option<&mut DisconnectMonitor>,
) -> Result<ClaudeCliCompletion, ClaudeCliError> {
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
    let output_limit = body
        .get("max_output_tokens")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
        .or_else(|| configured_limit(&candidate, "output_limit"));
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
    std::fs::write(&prompt_file, &cli_input.system_prompt)
        .map_err(|_| ClaudeCliError::Failure("claude_cli_temp_unavailable"))?;
    let cancelled = Arc::new(
        Cancellation::new().map_err(|_| ClaudeCliError::Failure("claude_cli_relay_unavailable"))?,
    );
    let request_started = Instant::now();
    let mut relay_model = None;
    let (stdout, provider_status) = if local_login {
        let local_home = local_home.as_deref().expect("checked local CLI home");
        let identity = verify_local_subscription(
            state,
            executable,
            local_home,
            &config_home,
            &cache_home,
            temp.path(),
            &working_directory,
            &cancelled,
            &mut monitor,
        )
        .inspect_err(|_| {
            state.backend.claude_quota.begin(None);
            state
                .backend
                .management_events
                .publish(crate::services::management_events::Change::Quota);
        })?;
        let generation = state.backend.claude_quota.begin(identity);
        state
            .backend
            .management_events
            .publish(crate::services::management_events::Change::Quota);
        let command = process::command(process::CommandConfig {
            executable: &executable.executable,
            child_path: &executable.child_path,
            model,
            effort,
            output_limit,
            prompt_file: &prompt_file,
            schema: &schema,
            input_format: cli_input.input_format,
            mode: process::InvocationMode::LocalLogin { home: local_home },
            config_home: &config_home,
            cache_home: &cache_home,
            temp: temp.path(),
            working_directory: &working_directory,
        });
        let output = process::run_observed(
            command,
            Arc::clone(&stdin),
            &cancelled,
            || cancellation_reason(state, &mut monitor),
            |stdout| {
                if state
                    .backend
                    .claude_quota
                    .record(generation, stdout, crate::util::system_now())
                {
                    quota_query::publish(state);
                }
            },
        )
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
            output_limit,
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
                cancelled.cancel();
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
        relay_model = Some(relay_result.observation);
        (output, relay_result.status)
    };
    let mut cli_output = projection::parse_cli_result(&stdout).map_err(|code| {
        if local_login && code == "claude_cli_result_error" {
            ClaudeCliError::Failure("claude_cli_local_request_failed")
        } else {
            ClaudeCliError::Failure(code)
        }
    })?;
    if let Some(observation) = relay_model {
        cli_output["_emp_model_observation"] = observation.project("");
    }
    let response = projection::response_from_cli(&cli_output, &candidate, body, provider_status)?;
    Ok(ClaudeCliCompletion {
        response,
        route: candidate,
        request_started,
    })
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
        .then(|| {
            if monitor.is_interrupted() {
                process::CancellationReason::Steering
            } else {
                process::CancellationReason::DownstreamDisconnected
            }
        })
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
    cancelled: &Cancellation,
    monitor: &mut Option<&mut DisconnectMonitor>,
) -> Result<Option<String>, ClaudeCliError> {
    let command = process::auth_status_command(process::LocalCommandConfig {
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
                "claude_cli_interrupted" => ClaudeCliError::Failure("claude_cli_interrupted"),
                "claude_cli_timeout" => ClaudeCliError::Failure("claude_cli_auth_status_timeout"),
                _ => ClaudeCliError::Failure("claude_cli_auth_status_unavailable"),
            })?;
    let status = auth::parse_status(&stdout);
    match status {
        auth::LoginStatus::SubscriptionOAuth => Ok(quota::identity(&stdout)),
        auth::LoginStatus::Missing => Err(ClaudeCliError::Failure("claude_cli_login_required")),
        auth::LoginStatus::ApiKey => {
            Err(ClaudeCliError::Failure("claude_cli_subscription_required"))
        }
        auth::LoginStatus::Unknown => {
            Err(ClaudeCliError::Failure("claude_cli_auth_status_unknown"))
        }
    }
}
