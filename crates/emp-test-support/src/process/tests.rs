use super::*;
use crate::{TestBudget, test_timeout};
use std::{
    io::{Read, Write},
    net::TcpStream,
};

const CASE: &str = "EMP_TEST_OWNED_CHILD_CASE";
pub(super) const ADDRESS: &str = "EMP_TEST_OWNED_CHILD_ADDRESS";
const PIPE_PAYLOAD: usize = 1_048_576;
const PAYLOAD_START: &[u8] = b"EMP-PIPE-PAYLOAD\0";

pub(super) fn fixture(case: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "process::tests::child_fixture", "--nocapture"])
        .env(CASE, case);
    command
}

#[test]
fn child_fixture() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    match case.as_str() {
        "exit" => {
            write_fixture_output(b"normal stdout\n", b"normal stderr\n");
            std::process::exit(7);
        }
        "wait" => {
            println!("waiting stdout");
            eprintln!("blocked stderr");
            io::stdin().read_to_end(&mut Vec::new()).unwrap();
        }
        "large" => {
            write_fixture_output(&vec![b'O'; PIPE_PAYLOAD], &vec![b'E'; PIPE_PAYLOAD]);
            std::process::exit(0);
        }
        "parent" => {
            let _descendant = fixture("descendant").spawn().unwrap();
            std::process::exit(23);
        }
        "descendant" => {
            let socket = TcpStream::connect(std::env::var(ADDRESS).unwrap()).unwrap();
            println!("descendant ready");
            let lifetime = Deadline::after(test_timeout(Duration::from_secs(10)));
            let _ = lifetime.io(&socket).read(&mut [0]);
        }
        "capture-escaped-child" => super::capture::tests::check_escaped_child_capture(),
        "escaped" => {
            #[cfg(unix)]
            rustix::process::setsid().unwrap();
            let lifetime = Deadline::after(test_timeout(Duration::from_secs(10)));
            let address = std::env::var(ADDRESS).unwrap().parse().unwrap();
            let socket = lifetime.connect(address).unwrap();
            write_fixture_output(b"escaped stdout\n", b"escaped stderr\n");
            if lifetime.io(socket).read_exact(&mut [0]).is_err() {
                std::process::exit(25);
            }
            let stdout = io::stdout().lock().write_all(b"probe\n");
            let stderr = io::stderr().lock().write_all(b"probe\n");
            let pipes_closed = [stdout, stderr].into_iter().all(
                |result| matches!(result, Err(error) if error.kind() == io::ErrorKind::BrokenPipe),
            );
            std::process::exit(if pipes_closed { 0 } else { 24 });
        }
        "invalid-time-scale" => {
            let panic = std::panic::catch_unwind(|| {
                let _child = OwnedChild::spawn(fixture("exit")).expect("start child fixture");
                panic!("OwnedChild accepted invalid EMP_TEST_TIME_SCALE");
            })
            .unwrap_err();
            assert_eq!(
                panic
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| panic.downcast_ref::<String>().map(String::as_str)),
                Some("EMP_TEST_TIME_SCALE must be a number between 0.0625 and 64 inclusive")
            );
        }
        _ => panic!("unknown child fixture"),
    }
}

fn write_fixture_output(stdout: &[u8], stderr: &[u8]) {
    let mut error = io::stderr().lock();
    error.write_all(PAYLOAD_START).unwrap();
    error.write_all(stderr).unwrap();
    error.flush().unwrap();
    let mut output = io::stdout().lock();
    output.write_all(PAYLOAD_START).unwrap();
    output.write_all(stdout).unwrap();
    output.flush().unwrap();
}

pub(super) fn assert_payload(captured: &[u8], expected: &[u8]) {
    let mark = captured
        .windows(PAYLOAD_START.len())
        .position(|window| window == PAYLOAD_START)
        .expect("fixture payload start");
    let payload = &captured[mark + PAYLOAD_START.len()..];
    assert_eq!(payload.len(), expected.len());
    assert!(payload == expected, "fixture payload bytes differ");
}

fn fixture_output(case: &str, budget: TestBudget) -> Output {
    OwnedChild::output(fixture(case), Deadline::after(budget)).unwrap()
}

#[test]
fn captures_exit_status_and_both_streams() {
    let output = fixture_output("exit", test_timeout(Duration::from_secs(5)));
    assert_eq!(output.status.code(), Some(7));
    assert_payload(&output.stdout, b"normal stdout\n");
    assert_payload(&output.stderr, b"normal stderr\n");
}

#[test]
fn invalid_time_scale_panics_during_construction_without_aborting() {
    for scale in ["bad", "2e18"] {
        let mut command = fixture("invalid-time-scale");
        command.env("EMP_TEST_TIME_SCALE", scale);
        let output = OwnedChild::output(
            command,
            Deadline::after(test_timeout(Duration::from_secs(5))),
        )
        .unwrap();
        assert!(
            output.status.success(),
            "scale {scale}: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn deadline_kills_and_reaps_a_hung_child_and_retains_both_streams() {
    let mut command = fixture("wait");
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command).unwrap();
    let _input = child.take_stdin().unwrap();
    let ready = Deadline::after(test_timeout(Duration::from_secs(5)));
    while !child
        .stdout_line(ready)
        .unwrap()
        .unwrap()
        .contains("waiting stdout")
    {}
    assert_eq!(
        child.stderr_line(ready).unwrap().as_deref(),
        Some("blocked stderr")
    );
    let error = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(2))))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(!error.capture_timed_out());
    assert!(!error.status().unwrap().success());
    assert!(String::from_utf8_lossy(error.stdout()).contains("waiting stdout"));
    assert!(String::from_utf8_lossy(error.stderr()).contains("blocked stderr"));
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(
            !text.contains("waiting stdout") && !text.contains("blocked stderr"),
            "{text}"
        );
    }
    #[cfg(unix)]
    {
        use rustix::{
            io::Errno,
            process::{Pid, WaitOptions, waitpid},
        };
        assert!(matches!(
            waitpid(Some(Pid::from_child(&child.child)), WaitOptions::NOHANG),
            Err(Errno::CHILD)
        ));
    }
}

#[test]
fn drains_more_than_a_pipe_buffer_from_each_stream() {
    let output = fixture_output("large", test_timeout(Duration::from_secs(10)));
    assert!(output.status.success());
    assert_payload(&output.stdout, &vec![b'O'; PIPE_PAYLOAD]);
    assert_payload(&output.stderr, &vec![b'E'; PIPE_PAYLOAD]);
}
