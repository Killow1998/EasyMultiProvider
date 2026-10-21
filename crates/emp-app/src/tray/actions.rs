//! Serialize menu actions off the UI thread; never bypass integration admission.
use super::{Action, Mode, Snapshot};
use crate::app::ServerState;
use crate::services::integration::commands::{self, Body};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Duration;

#[cfg(test)]
mod tests;

pub(super) fn snapshot(state: &ServerState, busy: bool) -> Snapshot {
    let english = state
        .backend
        .configuration
        .read()
        .is_ok_and(|config| config["language"] == "en");
    let mode = match state.backend.integration.manager.status() {
        Ok(status) => match (status.state.as_str(), status.relation.as_str()) {
            ("active", "applied") => Mode::Emp,
            ("native" | "restored", "unleased" | "original") => Mode::Native,
            _ => Mode::Unresolved,
        },
        Err(_) => Mode::Unresolved,
    };
    Snapshot {
        english,
        mode,
        busy,
    }
}

pub(super) fn run(
    state: Arc<ServerState>,
    actions: mpsc::Receiver<Action>,
    updates: mpsc::SyncSender<Snapshot>,
) {
    while !state.shutdown.load(Ordering::Acquire) {
        let current = snapshot(&state, false);
        let _ = updates.try_send(current);
        let action = match actions.recv_timeout(Duration::from_millis(750)) {
            Ok(action) => action,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if action == Action::Open {
            if let Ok(url) = state.reopen_url() {
                if !crate::cli::desktop::open_browser(&url) {
                    crate::cli::desktop::notify_error(
                        current.copy().failure_title,
                        current.copy().browser_failed,
                    );
                }
            } else {
                crate::cli::desktop::notify_error(
                    current.copy().failure_title,
                    current.copy().operation_failed,
                );
            }
            continue;
        }
        let _ = updates.try_send(Snapshot {
            busy: true,
            ..current
        });
        let error = execute(&state, action);
        let _ = updates.try_send(snapshot(&state, false));
        if state.shutdown.load(Ordering::Acquire) {
            break;
        }
        if let Some(reason) = error {
            let copy = current.copy();
            let message = match reason.as_str() {
                "active_conversations" | "active_codex_writer" => copy.active_work,
                "empty_emp_catalog" => copy.empty_catalog,
                _ => copy.operation_failed,
            };
            crate::cli::desktop::notify_error(copy.failure_title, message);
        }
    }
}

fn execute(state: &ServerState, action: Action) -> Option<String> {
    if action == Action::Quit {
        return match crate::services::shutdown::prepare_quit(None, state, 0) {
            Ok(()) => {
                state.request_shutdown();
                None
            }
            Err(error) => Some(error.code.unwrap_or("quit_failed").to_owned()),
        };
    }
    let operation = match action {
        Action::Enable => "enable",
        Action::Restore => "restore",
        Action::Open | Action::Quit => return None,
    };
    let result = commands::execute(None, state, operation, true, 0);
    if result.status < 400 {
        return None;
    }
    Some(match result.body {
        Body::Summary(value) => value["error"]["code"]
            .as_str()
            .unwrap_or("integration_failed")
            .to_owned(),
        _ => "integration_failed".to_owned(),
    })
}
