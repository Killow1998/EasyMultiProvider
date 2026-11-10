//! Temporary Claude TUI per user turn; Codex owns history and tool execution.

mod auth;
mod cancellation;
mod context;
mod local_query;
pub(crate) mod model_query;
pub(crate) mod quota;
pub(crate) mod quota_query;
use cancellation::Cancellation;
mod instructions;
#[cfg(test)]
mod instructions_tests;
mod media;
mod output_budget;
mod process;
mod projection;
pub(crate) mod tui;

use crate::app::ServerState;
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace};
use emp_core::ResolvedRoute;
use emp_router::CompleteResponse;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod error;
pub(crate) use error::{ClaudeCliError, failure_details, failure_stage};

const CHILD_POLL: Duration = Duration::from_millis(25);

pub(crate) struct ClaudeCliCompletion {
    pub(crate) response: CompleteResponse,
    pub(crate) route: ResolvedRoute,
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

pub(crate) use context::{assess_context, context_estimation_payload};

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
    usage: &mut crate::services::request_outcome::RequestOutcome<'_>,
    text: &mut dyn FnMut(&str, &str) -> std::io::Result<()>,
) -> Result<ClaudeCliCompletion, ClaudeCliError> {
    let started = Instant::now();
    let result = (|| {
        if state.shutdown.load(Ordering::Acquire) {
            return Err(ClaudeCliError::ShuttingDown);
        }
        preflight_input(body)?;
        let executable = emp_codex::installed_cli::resolve_claude_cli()
            .ok_or(ClaudeCliError::Failure("claude_cli_unavailable"))?;
        tui::execute(
            state,
            route,
            body,
            incoming,
            &executable,
            monitor,
            usage,
            text,
        )
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
        usage.claude_error(error);
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
                "claude_cli_capacity_unavailable" => ClaudeCliError::Failure(code),
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
