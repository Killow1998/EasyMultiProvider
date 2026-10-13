//! Bounded stdout/stderr, concurrent input and prompt cancellation of owned CLI.
use super::Error;
use crate::services::child_process::ChildTree;
use std::io::{Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) fn run(
    mut command: Command,
    input: &[u8],
    timeout: Duration,
    mut cancelled: impl FnMut() -> Option<Error>,
) -> Result<Vec<u8>, Error> {
    let exceeded = AtomicBool::new(false);
    let local_tool = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let child = command.spawn().map_err(|_| Error("agy_cli_spawn_failed"))?;
        let mut tree =
            ChildTree::new(child).map_err(|_| Error("agy_cli_process_tree_unavailable"))?;
        let stdout = tree
            .child
            .stdout
            .take()
            .ok_or(Error("agy_cli_pipe_failed"))?;
        let stderr = tree
            .child
            .stderr
            .take()
            .ok_or(Error("agy_cli_pipe_failed"))?;
        let mut stdin = tree
            .child
            .stdin
            .take()
            .ok_or(Error("agy_cli_pipe_failed"))?;
        let output = scope.spawn(|| drain(stdout, 4 * 1024 * 1024, &exceeded, Some(&local_tool)));
        let errors = scope.spawn(|| drain(stderr, 64 * 1024, &exceeded, None));
        let writer = scope.spawn(move || stdin.write_all(input));
        let deadline = Instant::now() + timeout;
        let failure = loop {
            if let Some(error) = cancelled() {
                break Some(error);
            }
            if exceeded.load(Ordering::Acquire) {
                break Some(Error("agy_cli_output_too_large"));
            }
            if local_tool.load(Ordering::Acquire) {
                break Some(Error("agy_cli_local_tool_attempt"));
            }
            if Instant::now() >= deadline {
                break Some(Error("agy_cli_timeout"));
            }
            match tree.child.try_wait() {
                Ok(Some(_)) => break None,
                Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                Err(_) => break Some(Error("agy_cli_process_failed")),
            }
        };
        tree.stop();
        let status = tree.wait().ok();
        let written = writer.join().ok().is_some_and(|r| r.is_ok());
        let stdout = output
            .join()
            .ok()
            .and_then(Result::ok)
            .ok_or(Error("agy_cli_pipe_failed"))?;
        let stderr = errors
            .join()
            .ok()
            .and_then(Result::ok)
            .ok_or(Error("agy_cli_pipe_failed"))?;
        if let Some(error) = failure {
            return Err(error);
        }
        if exceeded.load(Ordering::Acquire) {
            return Err(Error("agy_cli_output_too_large"));
        }
        if local_tool.load(Ordering::Acquire) {
            return Err(Error("agy_cli_local_tool_attempt"));
        }
        if !status.is_some_and(|s| s.success()) {
            let text = String::from_utf8_lossy(&stderr).to_ascii_lowercase();
            return Err(Error(
                if text.contains("authentication required") || text.contains("not authenticated") {
                    "agy_cli_login_required"
                } else if text.contains("invalid model selection") {
                    "agy_cli_model_unavailable"
                } else {
                    "agy_cli_process_failed"
                },
            ));
        }
        if !written {
            return Err(Error("agy_cli_stdin_failed"));
        }
        Ok(stdout)
    })
}

fn drain(
    mut reader: impl Read,
    limit: usize,
    exceeded: &AtomicBool,
    local_tool: Option<&AtomicBool>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut parsed = 0;
    let mut chunk = [0; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(output);
        }
        let keep = count.min(limit.saturating_sub(output.len()));
        let previous_len = output.len();
        output.extend_from_slice(&chunk[..keep]);
        if let Some(attempted) = local_tool {
            for end in (previous_len..output.len()).filter(|index| output[*index] == b'\n') {
                let line = &output[parsed..end];
                if serde_json::from_slice(line)
                    .ok()
                    .is_some_and(|event| super::projection::local_tool_attempt(&event))
                {
                    attempted.store(true, Ordering::Release);
                }
                parsed = end + 1;
            }
        }
        if keep != count {
            exceeded.store(true, Ordering::Release);
        }
    }
}
