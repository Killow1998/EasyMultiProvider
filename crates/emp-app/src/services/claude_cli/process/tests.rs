use super::*;
use std::process::Stdio;

#[cfg(unix)]
#[test]
fn timeout_and_disconnect_stop_the_child_process_group() {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("sh");
    command
        .args(["-c", "sleep 10"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let cancelled = Cancellation::new().unwrap();
    assert_eq!(
        run_with_timeout_policy(
            command,
            Arc::from(&b"input"[..]),
            &cancelled,
            Duration::from_millis(20),
            false,
            || None,
            |_| {}
        ),
        Err("claude_cli_timeout")
    );

    let mut command = Command::new("sh");
    command
        .args(["-c", "sleep 10"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let cancelled = Cancellation::new().unwrap();
    assert_eq!(
        run_with_timeout_policy(
            command,
            Arc::from(&b"input"[..]),
            &cancelled,
            Duration::from_secs(1),
            false,
            || Some(CancellationReason::DownstreamDisconnected),
            |_| {}
        ),
        Err("claude_cli_disconnected")
    );
}

#[cfg(unix)]
#[test]
fn normal_parent_exit_kills_descendant_holding_child_pipes_open() {
    use std::os::unix::process::CommandExt;

    let mut command = Command::new("sh");
    command
        // Consume the submitted input before exiting so this exercises
        // descendant pipe cleanup, not a race with the stdin writer.
        .args(["-c", "cat >/dev/null; (sleep 10) & exit 0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let cancelled = Cancellation::new().unwrap();
    let started = Instant::now();
    assert_eq!(
        run_with_timeout_policy(
            command,
            Arc::from(&b"input"[..]),
            &cancelled,
            Duration::from_secs(2),
            false,
            || None,
            |_| {}
        ),
        Ok(Vec::new())
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a descendant holding inherited pipes must be killed after normal parent exit"
    );
}

#[cfg(unix)]
#[test]
fn output_budget_event_stops_a_waiting_cli_without_waiting_for_timeout() {
    use std::os::unix::process::CommandExt;

    for event in [
        r#"{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}}"#,
        r#"{"type":"assistant","message":{"type":"message","stop_reason":"max_tokens"}}"#,
    ] {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "cat >/dev/null; printf '%s\\n' \"$1\"; exec sleep 10",
                "fixture",
                event,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let cancelled = Cancellation::new().unwrap();
        assert_eq!(
            run_with_timeout_policy(
                command,
                Arc::from(&b"input"[..]),
                &cancelled,
                Duration::from_secs(2),
                false,
                || None,
                |_| {}
            ),
            Err(super::super::output_budget::ERROR)
        );
    }
}
