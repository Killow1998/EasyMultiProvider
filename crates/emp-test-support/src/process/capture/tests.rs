use super::{
    super::{
        OwnedChild,
        tests::{ADDRESS, fixture},
    },
    *,
};
use crate::test_timeout;
use std::{
    net::{Ipv4Addr, TcpListener},
    process::Stdio,
    time::Duration,
};

#[test]
fn deadline_terminates_a_descendant_and_joins_both_output_readers() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut command = fixture("parent");
    command
        .env(ADDRESS, listener.local_addr().unwrap().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command).unwrap();
    let ready = Deadline::after(test_timeout(Duration::from_secs(5)));
    while !child
        .stdout_line(ready)
        .unwrap()
        .unwrap()
        .contains("descendant ready")
    {}
    listener.set_nonblocking(true).unwrap();
    let (socket, _) = crate::accept_blocking(&listener).unwrap();
    let readers =
        [&child.stdout, &child.stderr].map(|capture| Arc::clone(&capture.as_ref().unwrap().shared));
    let closed = Deadline::after(test_timeout(Duration::from_secs(5)));
    let error = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(2))))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(!error.capture_timed_out());
    assert_eq!(error.status().unwrap().code(), Some(23));
    for (name, reader) in ["stdout", "stderr"].into_iter().zip(readers) {
        assert!(
            reader.output.lock().unwrap().completion.is_some(),
            "{name} reader did not close its pipe"
        );
    }
    assert!(String::from_utf8_lossy(error.stdout()).contains("descendant ready"));
    assert_eq!(closed.io(&socket).read(&mut [0]).unwrap(), 0);
}

#[test]
fn capture_timeout_joins_readers_and_closes_pipes_held_by_an_escaped_child() {
    let output = fixture("capture-escaped-child").output().unwrap();
    assert!(
        output.status.success(),
        "{}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(in crate::process) fn check_escaped_child_capture() {
    use super::super::tests::assert_payload;
    use std::{io::Write, process::Child};

    struct EscapedChild(Child);

    impl Drop for EscapedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = fixture("escaped");
    command
        .env(ADDRESS, listener.local_addr().unwrap().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
        command.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    }
    let launched = command.spawn();
    #[cfg(windows)]
    if launched.as_ref().is_err_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
        )
    }) {
        eprintln!("Windows runner does not permit CREATE_BREAKAWAY_FROM_JOB");
        return;
    }
    let mut child = EscapedChild(launched.unwrap());
    let stdout = Capture::start(child.0.stdout.take().unwrap()).unwrap();
    let stderr = Capture::start(child.0.stderr.take().unwrap()).unwrap();
    let ready = Deadline::after(test_timeout(Duration::from_secs(5)));
    for (capture, payload) in [(&stdout, "escaped stdout"), (&stderr, "escaped stderr")] {
        while !capture.line(ready).unwrap().unwrap().ends_with(payload) {}
    }
    let (socket, _) = crate::accept_blocking(&listener).unwrap();

    let expired = Deadline::after(test_timeout(Duration::ZERO));
    for (capture, payload) in [
        (stdout, b"escaped stdout\n".as_slice()),
        (stderr, b"escaped stderr\n".as_slice()),
    ] {
        let reader = Arc::clone(&capture.shared);
        let (bytes, result) = capture.finish(expired);
        assert_eq!(result, Err(Failure::CaptureTimeout));
        assert_eq!(Arc::strong_count(&reader), 1, "output reader still running");
        assert_payload(&bytes, payload);
    }

    ready.io(socket).write_all(b"x").unwrap();
    let status = child.0.wait().unwrap();
    assert!(
        status.success(),
        "escaped child pipe probe failed: {status} (24: pipe open, 25: probe not received)"
    );
}
