//! Official agy headless adapter. Each invocation owns one disposable workspace
//! and one CLI conversation; Codex owns history and every workspace tool action.
mod error;
mod invocation;
mod models;
mod process;
mod profile;
mod projection;
mod tools;
pub(crate) use error::Error;
pub(crate) use models::discover;
pub(crate) use projection::preflight;

use crate::app::ServerState;
use crate::services::disconnect::{DisconnectMonitor, DisconnectRace};
use crate::services::request_outcome::RequestOutcome;
use emp_core::ResolvedRoute;
use emp_router::CompleteResponse;
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::time::Duration;

pub(crate) fn selected(route: &ResolvedRoute) -> bool {
    route.provider.value()["execution_backend"] == "agy_cli"
}

pub(crate) fn execute(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    mut monitor: Option<&mut DisconnectMonitor>,
    usage: &mut RequestOutcome<'_>,
) -> Result<CompleteResponse, Error> {
    let result = (|| {
        let input = projection::prepare(body)?;
        let invocation = invocation::Invocation::new(&input.instructions)?;
        let command = invocation.inference(&route.upstream_model, projection::effort(body)?);
        usage.dispatch();
        let output = process::run(command, &input.stdin, Duration::from_secs(300), || {
            cancellation(state, &mut monitor)
        })?;
        projection::complete(&output, body, &input.tools)
    })();
    if let Err(error) = result {
        usage.agy_error(error);
    }
    result
}

fn cancellation(
    state: &ServerState,
    monitor: &mut Option<&mut DisconnectMonitor>,
) -> Option<Error> {
    if state.shutdown.load(Ordering::Acquire) {
        return Some(Error("server_shutting_down"));
    }
    monitor.as_deref_mut().and_then(|monitor| {
        matches!(
            state.backend.transport.runtime.block_on(async {
                monitor
                    .race(tokio::time::sleep(Duration::from_millis(25)))
                    .await
            }),
            DisconnectRace::Disconnected
        )
        .then(|| {
            Error(if monitor.is_interrupted() {
                "agy_cli_interrupted"
            } else {
                "client_disconnected"
            })
        })
    })
}

#[cfg(test)]
mod tests;
