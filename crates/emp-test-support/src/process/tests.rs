use super::*;
use crate::{TestBudget, test_timeout};
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
};

const CASE: &str = "EMP_TEST_OWNED_CHILD_CASE";
const ADDRESS: &str = "EMP_TEST_OWNED_CHILD_ADDRESS";
const PIPE_PAYLOAD: usize = 1_048_576;

fn fixture(case: &str) -> Command {
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
            println!("normal stdout");
            eprintln!("normal stderr");
            std::process::exit(7);
        }
        "wait" => {
            println!("waiting stdout");
            eprintln!("blocked stderr");
            io::stdin().read_to_end(&mut Vec::new()).unwrap();
        }
        "large" => {
            io::stderr().write_all(&vec![b'E'; PIPE_PAYLOAD]).unwrap();
            io::stdout().write_all(&vec![b'O'; PIPE_PAYLOAD]).unwrap();
            io::stdout().flush().unwrap();
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
        _ => panic!("unknown child fixture"),
    }
}

fn fixture_output(case: &str, budget: TestBudget) -> Output {
    OwnedChild::output(&mut fixture(case), Deadline::after(budget)).unwrap()
}

#[test]
fn captures_exit_status_and_both_streams() {
    let output = fixture_output("exit", test_timeout(Duration::from_secs(5)));
    assert_eq!(output.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&output.stdout).contains("normal stdout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("normal stderr"));
}

#[test]
fn deadline_kills_and_reaps_a_hung_child_and_retains_both_streams() {
    let mut child = OwnedChild::spawn(
        fixture("wait")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap();
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
    assert!(output.stdout.ends_with(&vec![b'O'; PIPE_PAYLOAD]));
    assert!(output.stderr.ends_with(&vec![b'E'; PIPE_PAYLOAD]));
}

#[test]
fn deadline_terminates_a_descendant_that_still_holds_the_output_pipes() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut child = OwnedChild::spawn(
        fixture("parent")
            .env(ADDRESS, listener.local_addr().unwrap().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .unwrap();
    let ready = Deadline::after(test_timeout(Duration::from_secs(5)));
    while !child
        .stdout_line(ready)
        .unwrap()
        .unwrap()
        .contains("descendant ready")
    {}
    listener.set_nonblocking(true).unwrap();
    let (socket, _) = crate::accept_blocking(&listener).unwrap();
    let closed = Deadline::after(test_timeout(Duration::from_secs(5)));
    let error = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(2))))
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(error.status().unwrap().code(), Some(23));
    assert_eq!(closed.io(&socket).read(&mut [0]).unwrap(), 0);
}
