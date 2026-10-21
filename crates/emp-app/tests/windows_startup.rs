#![cfg(windows)]

#[allow(dead_code)]
mod support;

use std::os::windows::process::CommandExt;
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
            let output = workspace
                .command()
                .arg(argument)
                .creation_flags(flags)
                .output()
                .expect("run CLI command");
            assert_eq!(output.status.code(), Some(code));
            let text = if stream { output.stdout } else { output.stderr };
            assert!(String::from_utf8_lossy(&text).contains(expected));
        }
    }
}
