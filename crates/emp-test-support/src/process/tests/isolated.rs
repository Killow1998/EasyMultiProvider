use super::super::{POLL_INTERVAL, poll_pause};
use crate::{Deadline, fixture_root};
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
    process::{Command, Output, Stdio},
};
use tempfile::NamedTempFile;

const RESULT_PREFIX: &str = "EMP-ISOLATED: ";

pub(crate) fn command(test_name: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("locate test executable"));
    command.args(["--exact", test_name, "--nocapture"]);
    command
}

pub(crate) fn report(result: Result<(), &str>) {
    match result {
        Ok(()) => println!("\n{RESULT_PREFIX}done"),
        Err(reason) => println!("\n{RESULT_PREFIX}skipped {reason}"),
    }
}

pub(crate) fn run(mut command: Command, deadline: Deadline) -> Output {
    let root = fixture_root().expect("resolve test fixture root");
    let (stdout, stdout_path) = NamedTempFile::new_in(&root)
        .expect("isolated test stdout file")
        .into_parts();
    let (stderr, stderr_path) = NamedTempFile::new_in(&root)
        .expect("isolated test stderr file")
        .into_parts();
    command.stdin(Stdio::null()).stdout(stdout).stderr(stderr);
    let mut child = command.spawn().expect("spawn isolated test");
    let status = loop {
        let remaining = match deadline.accept(deadline.remaining(), "isolated test exit") {
            Ok(remaining) => remaining,
            Err(error) => break Err(error),
        };
        match child.try_wait() {
            Ok(Some(status)) => break deadline.accept(status, "isolated test exit"),
            Ok(None) => poll_pause(remaining.min(POLL_INTERVAL)),
            Err(error) => break Err(error),
        }
    };
    let status = status.map_err(|error| match child.kill() {
        Ok(()) => {
            child.wait().expect("reap isolated test");
            error
        }
        Err(kill_error) => match child.try_wait() {
            Ok(Some(_)) => error,
            Ok(None) => io::Error::new(
                kill_error.kind(),
                format!("terminate isolated test: {kill_error}"),
            ),
            Err(wait_error) => io::Error::new(
                kill_error.kind(),
                format!(
                    "terminate isolated test: {kill_error}\ncheck isolated test exit: {wait_error}"
                ),
            ),
        },
    });

    let stdout = read_output(&stdout_path);
    let stderr = read_output(&stderr_path);
    let stdout_text = String::from_utf8_lossy(&stdout);
    let stderr_text = String::from_utf8_lossy(&stderr);
    let diagnostics = || format!("{command:?}\n{stdout_text}\n{stderr_text}");
    let status = status.unwrap_or_else(|error| panic!("{error}\n{}", diagnostics()));
    assert!(status.success(), "{status}\n{}", diagnostics());
    assert!(
        stdout_text.lines().any(|line| line == "running 1 test"),
        "expected libtest to run exactly one test\n{}",
        diagnostics()
    );
    let mut results = stdout_text
        .lines()
        .chain(stderr_text.lines())
        .filter_map(|line| line.strip_prefix(RESULT_PREFIX));
    match (results.next(), results.next()) {
        (Some("done"), None) => {}
        (Some(result), None)
            if result
                .strip_prefix("skipped ")
                .is_some_and(|reason| !reason.trim().is_empty()) =>
        {
            eprintln!("{command:?}: {result}");
        }
        _ => panic!(
            "expected exactly one isolated test result\n{}",
            diagnostics()
        ),
    }
    Output {
        status,
        stdout,
        stderr,
    }
}

fn read_output(path: &Path) -> Vec<u8> {
    let file = File::open(path).expect("open isolated test output");
    let length = file.metadata().expect("isolated test output length").len();
    let mut bytes = Vec::new();
    file.take(length)
        .read_to_end(&mut bytes)
        .expect("read isolated test output");
    bytes
}

#[test]
fn reading_diagnostics_preserves_live_child_output() {
    use super::fixture;
    use crate::{OwnedChild, test_timeout};
    use std::time::Duration;

    let stdout = NamedTempFile::new_in(fixture_root().unwrap()).unwrap();
    let mut command = fixture("diagnostics");
    command
        .stdin(Stdio::piped())
        .stdout(stdout.as_file().try_clone().unwrap())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command).unwrap();
    let ready = Deadline::after(test_timeout(Duration::from_secs(5)));
    assert_eq!(child.stderr_line(ready).unwrap().as_deref(), Some("ready"));

    let length = stdout.as_file().metadata().unwrap().len();
    stdout.as_file().set_len(length + 1).unwrap();
    let snapshot = read_output(stdout.path());
    let prefix = snapshot.strip_suffix(b"\0").unwrap();
    assert!(prefix.ends_with(b"before diagnostics\n"));

    let exit = Deadline::after(test_timeout(Duration::from_secs(5)));
    assert!(child.wait_until(exit).unwrap().status.success());
    let diagnostics = read_output(stdout.path());
    assert!(
        diagnostics.starts_with(&[prefix, b"after diagnostics\n"].concat()),
        "{:?}",
        String::from_utf8_lossy(&diagnostics)
    );
}
