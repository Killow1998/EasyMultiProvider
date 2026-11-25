#![cfg(windows)]

#[allow(
    dead_code,
    reason = "This Windows target uses only startup-related shared fixtures"
)]
mod support;

use emp_test_support::{Deadline, OwnedChild, test_timeout};
use std::{os::windows::process::CommandExt, time::Duration};
use support::Workspace;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

#[test]
fn cli_output_and_errors_survive_with_or_without_a_console() {
    let workspace = Workspace::new();
    for flags in [0, CREATE_NO_WINDOW] {
        for (argument, code, stream, expected) in [
            ("--version", 0, true, "EMP "),
            ("--help", 0, true, "EMP"),
            (
                "unknown-command",
                2,
                false,
                "unknown command: unknown-command",
            ),
        ] {
            let output = OwnedChild::output(
                workspace.command().arg(argument).creation_flags(flags),
                Deadline::after(test_timeout(Duration::from_secs(5))),
            )
            .expect("run CLI command");
            assert_eq!(output.status.code(), Some(code));
            let text = if stream { output.stdout } else { output.stderr };
            assert!(String::from_utf8_lossy(&text).contains(expected));
        }
    }
}
