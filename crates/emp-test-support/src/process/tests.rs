use super::*;
use crate::test_timeout;
use rustix::{
    io::Errno,
    process::{Pid, WaitOptions, waitpid},
};

#[test]
fn captures_exit_status_and_both_streams() {
    let output = OwnedChild::output(
        Command::new("sh").args(["-c", "printf hello; printf error >&2; exit 7"]),
        Deadline::after(test_timeout(Duration::from_secs(5))),
    )
    .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"hello");
    assert_eq!(output.stderr, b"error");
}

#[test]
fn deadline_kills_and_reaps_a_hung_child_and_reports_its_output() {
    let mut child = Command::new("sh")
        .args(["-c", "printf waiting; printf blocked >&2; exec cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let pid = Pid::from_child(&child);
    let mut child = OwnedChild::new(child);
    let error = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(2))))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("stdout:\nwaiting"), "{error}");
    assert!(error.to_string().contains("stderr:\nblocked"), "{error}");
    assert!(matches!(
        waitpid(Some(pid), WaitOptions::NOHANG),
        Err(Errno::CHILD)
    ));
    drop(input);
}

#[test]
fn drains_more_than_a_pipe_buffer_from_each_stream() {
    let output = OwnedChild::output(
        Command::new("sh").args([
            "-c",
            "head -c 1048576 /dev/zero >&2; exec head -c 1048576 /dev/zero",
        ]),
        Deadline::after(test_timeout(Duration::from_secs(10))),
    )
    .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, vec![0; 1_048_576]);
    assert_eq!(output.stderr, vec![0; 1_048_576]);
}
