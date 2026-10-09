//! Bounded CLI execution, cancellation and ownership of the child process tree.
use super::cancellation::Cancellation;
use std::io::{Read, Write};
use std::process::{Child, Command};
use std::sync::Arc;

use std::thread;
use std::time::{Duration, Instant};

const MAX_CHILD_STDOUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_CHILD_STDERR_BYTES: usize = 1024 * 1024;
#[cfg(not(test))]
const CLI_TIMEOUT: Duration = Duration::from_secs(300);
#[cfg(test)]
const CLI_TIMEOUT: Duration = Duration::from_secs(10);
const CHILD_POLL: Duration = Duration::from_millis(25);
const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(5);

mod command;
pub(super) use command::{
    CommandConfig, InputFormat, InvocationMode, LocalCommandConfig, auth_status_command, command,
    control_command, host_cli_home,
};

pub(super) fn run_control(
    command: Command,
    input: &[u8],
    cancelled: &Cancellation,
    cancellation: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        Arc::from(input),
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

pub(super) fn run(
    command: Command,
    input: Arc<[u8]>,
    cancelled: &Cancellation,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        input,
        cancelled,
        CLI_TIMEOUT,
        false,
        cancellation_requested,
        |_| {},
    )
}

pub(super) fn run_observed(
    command: Command,
    input: Arc<[u8]>,
    cancelled: &Cancellation,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
    inspect_output: impl FnOnce(&[u8]),
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        input,
        cancelled,
        CLI_TIMEOUT,
        false,
        cancellation_requested,
        inspect_output,
    )
}

pub(super) fn run_auth_status(
    command: Command,
    cancelled: &Cancellation,
    cancellation_requested: impl FnMut() -> Option<CancellationReason>,
) -> Result<Vec<u8>, &'static str> {
    run_with_timeout_policy(
        command,
        Arc::from(&b""[..]),
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

fn run_with_timeout_policy(
    mut command: Command,
    input: Arc<[u8]>,
    cancelled: &Cancellation,
    timeout: Duration,
    allow_nonzero_exit: bool,
    mut cancellation_requested: impl FnMut() -> Option<CancellationReason>,
    inspect_output: impl FnOnce(&[u8]),
) -> Result<Vec<u8>, &'static str> {
    thread::scope(|scope| {
        let child = command.spawn().map_err(|_| "claude_cli_spawn_failed")?;
        let mut tree = ChildTree::new(child)?;
        let (stdout, stderr, stdin) = match (
            tree.child.stdout.take(),
            tree.child.stderr.take(),
            tree.child.stdin.take(),
        ) {
            (Some(stdout), Some(stderr), Some(stdin)) => (stdout, stderr, stdin),
            _ => {
                tree.stop();
                let _ = tree.wait();
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
            match tree.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
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
            thread::sleep(CHILD_POLL);
        }
        // Kill the entire process group/job before joining pipes. The CLI
        // parent may already have exited while a descendant still holds one.
        tree.stop();
        let status = tree.wait().ok();
        let input_ok = input_task.join().unwrap_or(false);
        let stdout = stdout_task.join().unwrap_or_default();
        let stderr = stderr_task.join().unwrap_or_default();
        let too_large = stdout.too_large || stderr.too_large;
        drop(stderr.bytes);
        if let Some(reason) = cancellation_reason {
            return Err(match reason {
                CancellationReason::DownstreamDisconnected => "claude_cli_disconnected",
                CancellationReason::ServerShutdown => "claude_cli_shutdown",
                CancellationReason::Steering => "claude_cli_interrupted",
            });
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

struct ChildTree {
    child: Child,
    stopped: bool,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl ChildTree {
    #[allow(unused_mut)]
    fn new(mut child: Child) -> Result<Self, &'static str> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW,
            };
            let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if job.is_null() || unsafe { AssignProcessToJobObject(job, child.as_raw_handle()) } == 0
            {
                if !job.is_null() {
                    unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err("claude_cli_process_tree_unavailable");
            }
            Ok(Self {
                child,
                stopped: false,
                job,
            })
        }
        #[cfg(not(windows))]
        {
            Ok(Self {
                child,
                stopped: false,
            })
        }
    }

    fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGTERM);
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1) };
        }
        #[cfg(unix)]
        {
            thread::sleep(Duration::from_millis(100));
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
        }
        let _ = self.child.kill();
    }

    fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }
}

impl Drop for ChildTree {
    fn drop(&mut self) {
        if !self.stopped {
            self.stop();
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        if !self.job.is_null() {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.job) };
        }
    }
}

#[cfg(test)]
mod tests;
