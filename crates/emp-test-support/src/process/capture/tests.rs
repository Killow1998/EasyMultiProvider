use super::{
    super::{
        OwnedChild,
        tests::{ADDRESS, fixture},
    },
    *,
};
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
