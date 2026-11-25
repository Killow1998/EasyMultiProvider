//! Bounded CLI execution, cancellation and ownership of the child process tree.
use super::cancellation::Cancellation;
use crate::process_tree::ProcessTree;
use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus};

use std::thread;
use std::time::{Duration, Instant};

const MAX_CHILD_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHILD_STDERR_BYTES: usize = 1024 * 1024;
const CHILD_POLL: Duration = Duration::from_millis(25);
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) mod admission;
mod command;
pub(super) use command::{LocalCommandConfig, auth_status_command, control_command, host_cli_home};

pub(super) fn run_control(
    command: Command,
    input: &[u8],
    cancelled: &Cancellation,
    cancellation: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        input.to_vec(),
        cancelled,
        Duration::from_secs(20),
        false,
        cancellation,
        |_| {},
    )
}

#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
    too_large: bool,
    budget_exhausted: bool,
}

pub(super) fn run_auth_status(
    command: Command,
    cancelled: &Cancellation,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        Vec::new(),
        cancelled,
        AUTH_STATUS_TIMEOUT,
        true,
        cancellation_requested,
        |_| {},
    )
}

#[derive(Clone, Copy)]
pub(super) enum CancellationReason {
    DownstreamDisconnected,
    ServerShutdown,
    Steering,
}

fn cancellation_error(reason: CancellationReason) -> &'static str {
    match reason {
        CancellationReason::DownstreamDisconnected => "claude_cli_disconnected",
        CancellationReason::ServerShutdown => "claude_cli_shutdown",
        CancellationReason::Steering => "claude_cli_interrupted",
    }
}

fn run_with_timeout_policy(
    mut command: Command,
    input: Vec<u8>,
    cancelled: &Cancellation,
    timeout: Duration,
    allow_nonzero_exit: bool,
    mut cancellation_requested: impl FnMut() -> Option<CancellationReason>,
    inspect_output: impl FnOnce(&[u8]),
) -> Result<Vec<u8>, &'static str> {
    let _permit = admission::acquire(cancelled, &mut cancellation_requested)?;
    thread::scope(|scope| {
        #[cfg(windows)]
        let creation_flags = windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP
            | windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        ProcessTree::configure(
            &mut command,
            #[cfg(windows)]
            creation_flags,
        );
        let mut child = command.spawn().map_err(|_| "claude_cli_spawn_failed")?;
        let tree = ProcessTree::attach(&child).map_err(|_| {
            let _ = child.kill();
            let _ = child.wait();
            "claude_cli_process_tree_unavailable"
        })?;
        let mut child = ControlChild { child, tree };
        let (stdout, stderr, stdin) = match (
            child.child.stdout.take(),
            child.child.stderr.take(),
            child.child.stdin.take(),
        ) {
            (Some(stdout), Some(stderr), Some(stdin)) => (stdout, stderr, stdin),
            _ => {
                let _ = child.stop();
                return Err("claude_cli_pipe_failed");
            }
        };
        let stdout_task = drain_pipe(scope, stdout, MAX_CHILD_STDOUT_BYTES, Some(cancelled));
        let stderr_task = drain_pipe(scope, stderr, MAX_CHILD_STDERR_BYTES, None);
        let input_task = scope.spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(&input).is_ok()
        });
        let mut timed_out = false;
        let mut cancellation_reason = None;
        let deadline = Instant::now() + timeout;
        loop {
            match child.tree.has_exited(&mut child.child) {
                Ok(true) => break,
                Ok(false) => {}
                Err(_) => {
                    cancelled.cancel();
                    break;
                }
            }
            if Instant::now() >= deadline {
                timed_out = true;
                cancelled.cancel();
                break;
            }
            if let Some(reason) = cancellation_requested() {
                cancellation_reason = Some(reason);
                cancelled.cancel();
                break;
            }
            if cancelled.is_cancelled() {
                break;
            }
            #[expect(
                clippy::disallowed_methods,
                reason = "子进程退出需保留取消或超时检查，按既有固定间隔轮询"
            )]
            thread::sleep(CHILD_POLL);
        }
        let status = if child.tree.is_stopped() {
            None
        } else {
            child.stop().ok()
        };
        let input_ok = input_task.join().unwrap_or(false);
        let stdout = stdout_task.join().unwrap_or_default();
        let stderr = stderr_task.join().unwrap_or_default();
        let too_large = stdout.too_large || stderr.too_large;
        drop(stderr.bytes);
        if let Some(reason) = cancellation_reason {
            return Err(cancellation_error(reason));
        }
        if timed_out {
            return Err("claude_cli_timeout");
        }
        if too_large {
            return Err("claude_cli_output_too_large");
        }
        if stdout.budget_exhausted {
            return Err(super::output_budget::ERROR);
        }
        if !input_ok {
            return Err("claude_cli_stdin_failed");
        }
        inspect_output(&stdout.bytes);
        if !allow_nonzero_exit && !status.is_some_and(|status| status.success()) {
            return Err("claude_cli_process_failed");
        }
        Ok(stdout.bytes)
    })
}

struct ControlChild {
    child: Child,
    tree: ProcessTree,
}

impl ControlChild {
    fn stop(&mut self) -> std::io::Result<ExitStatus> {
        self.tree.request_graceful_stop();
        #[cfg(windows)]
        let _ = self.child.kill();
        self.tree.wait(&mut self.child)
    }
}

impl Drop for ControlChild {
    fn drop(&mut self) {
        if !self.tree.is_stopped() {
            let _ = self.stop();
        }
    }
}

fn drain_pipe<'scope, R: Read + Send + 'scope>(
    scope: &'scope thread::Scope<'scope, '_>,
    mut pipe: R,
    max: usize,
    cancelled: Option<&'scope Cancellation>,
) -> thread::ScopedJoinHandle<'scope, BoundedOutput> {
    scope.spawn(move || {
        let mut bytes = Vec::new();
        let mut too_large = false;
        let mut budget_exhausted = false;
        let mut events = super::output_budget::Events::default();
        let mut buffer = [0_u8; 8192];
        loop {
            let Ok(read) = pipe.read(&mut buffer) else {
                break;
            };
            if read == 0 {
                break;
            }
            let remaining = max.saturating_sub(bytes.len());
            bytes.extend_from_slice(&buffer[..read.min(remaining)]);
            too_large |= read > remaining;
            if let Some(cancelled) = cancelled
                && !too_large
                && events.observe(&bytes)
            {
                budget_exhausted = true;
                cancelled.cancel();
            }
        }
        budget_exhausted |= cancelled.is_some() && events.finish(&bytes);
        BoundedOutput {
            bytes,
            too_large,
            budget_exhausted,
        }
    })
}

#[cfg(all(test, unix))]
mod tests;
