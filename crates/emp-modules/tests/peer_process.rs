#![cfg(unix)]

use emp_modules::peer::{CallError, ErrorCode, Handlers, Peer, PendingRequest, Side};
use emp_test_support::{Deadline, Observation, OwnedChild, test_timeout};
use lsp_server::{Message, Request, RequestId, Response};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{self, BufReader, PipeReader, PipeWriter, Write},
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

const CHILD_MODE: &str = "EMP_TEST_API3_PEER";

fn budget() -> Duration {
    test_timeout(Duration::from_secs(5)).duration()
}

fn module(mode: &str, handlers: Handlers) -> (Peer, OwnedChild) {
    let (reader, writer, child) = spawn_module(mode);
    let peer = Peer::connect(Side::Host, reader, writer, handlers).unwrap();
    (peer, child)
}

fn spawn_module(mode: &str) -> (BufReader<PipeReader>, PipeWriter, OwnedChild) {
    let (reader, stdout) = io::pipe().unwrap();
    let (stdin, writer) = io::pipe().unwrap();
    let mut command = Command::new("sh");
    command
        .args(["-c", r#"exec "$@" 3>&1 1>&2"#, "sh"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "module_fixture", "--nocapture"])
        .env(CHILD_MODE, mode)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(Stdio::null());
    let child = OwnedChild::spawn(command).unwrap();
    (BufReader::new(reader), writer, child)
}

#[test]
fn module_fixture() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    if mode == "broken-write" {
        check_broken_write_pipe();
        return;
    }
    let mut output = OpenOptions::new().write(true).open("/dev/fd/3").unwrap();
    let mut input = BufReader::new(io::stdin());
    if matches!(
        mode.as_str(),
        "oversized" | "wrong-prefix" | "duplicate-key" | "truncated"
    ) {
        let request = Message::read(&mut input).unwrap().unwrap();
        assert!(matches!(request, Message::Request(req) if req.id == "h:1".to_owned().into()));
        match mode.as_str() {
            "oversized" => output
                .write_all(b"Content-Length: 16777217\r\n\r\n")
                .unwrap(),
            "wrong-prefix" => Message::Response(Response::new_ok("m:1".to_owned().into(), true))
                .write(&mut output)
                .unwrap(),
            "duplicate-key" => {
                let json = r#"{"id":"h:1","result":[{"ready":false,"ready":true}]}"#;
                write!(output, "Content-Length: {}\r\n\r\n{json}", json.len()).unwrap();
            }
            "truncated" => {
                output.write_all(b"Content-Length: 100\r\n\r\n{").unwrap();
                output.flush().unwrap();
                return;
            }
            _ => unreachable!(),
        }
        assert!(Message::read(&mut input).unwrap().is_none());
        return;
    }
    let mut handlers = Handlers::default();
    handlers.register("module.echo", |params, _, _| Ok(params));
    handlers.register("module.callback", |params, token, peer| {
        let reply = peer.call("host.echo", params, budget()).unwrap();
        peer.notify("host.finished", json!([reply, token.is_cancelled()]))
            .unwrap();
        Ok(reply)
    });
    handlers.register("module.panic", |_, _, _| panic!("fixture handler panic"));
    let gates = Arc::new(Mutex::new(HashMap::new()));
    let waiting = gates.clone();
    handlers.register("module.gated", move |params, token, peer| {
        let key = params.as_u64().unwrap();
        let (release, gate) = mpsc::channel();
        waiting.lock().unwrap().insert(key, release);
        peer.notify("host.started", params.clone()).unwrap();
        gate.recv_timeout(budget()).unwrap();
        peer.notify("host.finished", json!([params, token.is_cancelled()]))
            .unwrap();
        Ok(params)
    });
    handlers.register("module.release", move |params, _, _| {
        gates
            .lock()
            .unwrap()
            .remove(&params.as_u64().unwrap())
            .unwrap()
            .send(())
            .unwrap();
        Ok(Value::Null)
    });
    handlers.register("module.exit", |_, _, _| std::process::exit(0));
    if mode == "cancellation" {
        let (reader, writer) = io::pipe().unwrap();
        let peer = Peer::connect(Side::Module, input, writer, handlers).unwrap();
        let mut reader = BufReader::new(reader);
        let mut finished = None;
        while let Some(message) = Message::read(&mut reader).unwrap() {
            if matches!(&message, Message::Notification(notification) if notification.method == "host.finished")
            {
                assert!(finished.replace(message).is_none());
                continue;
            }
            message.write(&mut output).unwrap();
            if let Message::Response(response) = &message
                && let Some(Message::Notification(marker)) = &finished
                && response.response_result.as_ref().ok() == marker.params.get(0)
            {
                finished.take().unwrap().write(&mut output).unwrap();
            }
        }
        peer.wait().unwrap();
    } else {
        let result = Peer::connect(Side::Module, input, output, handlers)
            .unwrap()
            .wait();
        if mode == "duplicate-id" {
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        } else {
            result.unwrap();
        }
    }
}

#[test]
fn a_module_calls_back_while_the_host_waits_and_unknown_methods_fail() {
    let mut handlers = Handlers::default();
    handlers.register("host.echo", |params, _, _| {
        Ok(json!(["host replied", params]))
    });
    let (peer, _child) = module("peer", handlers);
    let host = peer.handle();
    assert_eq!(
        host.call("module.callback", json!(17), budget()).unwrap(),
        json!(["host replied", 17])
    );
    assert!(matches!(
        host.call("module.unknown", Value::Null, budget()),
        Err(CallError::Remote(error)) if error.code == ErrorCode::MethodNotFound as i32
    ));
}

#[test]
fn out_of_order_responses_reach_the_correct_waiters() {
    let mut handlers = Handlers::default();
    let (started, starting) = mpsc::channel();
    handlers.register("host.started", move |params, _, _| {
        started.send(params).unwrap();
        Ok(Value::Null)
    });
    let (peer, _child) = module("peer", handlers);
    let host = peer.handle();
    let slow = host.start_request("module.gated", json!(1)).unwrap();
    assert_eq!(starting.recv_timeout(budget()).unwrap(), json!(1));
    let fast = host.start_request("module.echo", json!(2)).unwrap();
    assert_eq!(fast.wait(budget()).unwrap(), json!(2));
    host.call("module.release", json!(1), budget()).unwrap();
    assert_eq!(slow.wait(budget()).unwrap(), json!(1));
}

#[test]
fn cancellation_releases_the_waiter_before_the_handler_and_reaches_its_token() {
    let mut handlers = Handlers::default();
    let (started, starting) = mpsc::channel();
    let (finished, finishing) = mpsc::channel();
    handlers.register("host.started", move |params, _, _| {
        started.send(params).unwrap();
        Ok(Value::Null)
    });
    handlers.register("host.finished", move |params, _, _| {
        finished.send(params).unwrap();
        Ok(Value::Null)
    });
    let (peer, _child) = module("cancellation", handlers);
    let host = peer.handle();
    let request = host.start_request("module.gated", json!(1)).unwrap();
    assert_eq!(starting.recv_timeout(budget()).unwrap(), json!(1));
    cancel_while_waiting(request);
    host.call("module.release", json!(1), budget()).unwrap();
    assert_eq!(finishing.recv_timeout(budget()).unwrap(), json!([1, true]));
    assert_eq!(
        host.call("module.echo", json!(2), budget()).unwrap(),
        json!(2)
    );
}

#[test]
fn cancelling_a_parent_does_not_block_an_active_callback() {
    let mut handlers = Handlers::default();
    let (started, starting) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let (finished, finishing) = mpsc::channel();
    let gate = Mutex::new(gate);
    handlers.register("host.echo", move |params, _, _| {
        started.send(()).unwrap();
        gate.lock().unwrap().recv_timeout(budget()).unwrap();
        Ok(params)
    });
    handlers.register("host.finished", move |params, _, _| {
        finished.send(params).unwrap();
        Ok(Value::Null)
    });
    let (peer, _child) = module("cancellation", handlers);
    let host = peer.handle();
    let parent = host.start_request("module.callback", json!(5)).unwrap();
    starting.recv_timeout(budget()).unwrap();
    cancel_while_waiting(parent);
    host.call("module.echo", json!(null), budget()).unwrap();
    release.send(()).unwrap();
    assert_eq!(finishing.recv_timeout(budget()).unwrap(), json!([5, true]));
    assert_eq!(
        host.call("module.echo", json!(6), budget()).unwrap(),
        json!(6)
    );
}

fn cancel_while_waiting(request: PendingRequest) {
    let canceller = request.canceller();
    let (entered, entering) = mpsc::channel();
    let waiter = Observation::spawn(move |probe| {
        probe.observe(|deadline| {
            request.wait_registered(deadline.remaining(), || entered.send(()).unwrap())
        });
    });
    let result = waiter.begin(
        Deadline::after(test_timeout(Duration::from_secs(5))),
        "cancelled waiter",
    );
    entering.recv_timeout(budget()).unwrap();
    canceller.cancel();
    assert!(matches!(result.wait().unwrap(), Err(CallError::Cancelled)));
}

#[test]
fn child_exit_disconnects_every_waiter() {
    let mut handlers = Handlers::default();
    let (started, starting) = mpsc::channel();
    handlers.register("host.started", move |_, _, _| {
        started.send(()).unwrap();
        Ok(Value::Null)
    });
    let (peer, _child) = module("peer", handlers);
    let host = peer.handle();
    let first = host.start_request("module.gated", json!(1)).unwrap();
    let second = host.start_request("module.gated", json!(2)).unwrap();
    starting.recv_timeout(budget()).unwrap();
    starting.recv_timeout(budget()).unwrap();
    host.notify("module.exit", Value::Null).unwrap();
    assert!(matches!(first.wait(budget()), Err(CallError::Disconnected)));
    assert!(matches!(
        second.wait(budget()),
        Err(CallError::Disconnected)
    ));
    assert!(matches!(
        host.start_request("module.echo", Value::Null),
        Err(CallError::Disconnected)
    ));
}

#[test]
fn invalid_frames_disconnect_and_report_the_protocol_error() {
    for mode in ["wrong-prefix", "oversized", "duplicate-key"] {
        let (peer, mut child) = module(mode, Handlers::default());
        let host = peer.handle();
        assert!(
            matches!(
                host.call("module.echo", Value::Null, budget()),
                Err(CallError::Disconnected)
            ),
            "{mode}"
        );
        assert_eq!(
            peer.wait().unwrap_err().kind(),
            io::ErrorKind::InvalidData,
            "{mode}"
        );
        let exit = child
            .wait_until(Deadline::after(test_timeout(Duration::from_secs(5))))
            .unwrap();
        assert!(exit.status.success(), "{mode}");
    }
}

#[test]
fn truncated_frame_keeps_the_read_error_kind_and_public_message() {
    let (peer, mut child) = module("truncated", Handlers::default());
    assert!(matches!(
        peer.handle().call("module.echo", Value::Null, budget()),
        Err(CallError::Disconnected)
    ));
    let error = peer.wait().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(error.to_string(), "cannot read peer frame");
    assert!(
        child
            .wait_until(Deadline::after(test_timeout(Duration::from_secs(5))))
            .unwrap()
            .status
            .success()
    );
}

struct GateSecondFrame {
    pipe: PipeWriter,
    first_flush: bool,
    gate: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,
}

impl Write for GateSecondFrame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.first_flush
            && let Some((entered, release)) = self.gate.take()
        {
            entered.send(()).unwrap();
            release.recv_timeout(budget()).unwrap();
        }
        self.pipe.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.pipe.flush()?;
        self.first_flush = true;
        Ok(())
    }
}

#[test]
fn a_broken_write_pipe_releases_unsent_requests_and_keeps_written_replies() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "module_fixture", "--nocapture"])
        .env(CHILD_MODE, "broken-write");
    let output = OwnedChild::output(
        command,
        Deadline::after(test_timeout(Duration::from_secs(15))),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn check_broken_write_pipe() {
    let exited;
    let (input, writer) = io::pipe().unwrap();
    let (reader, mut output) = io::pipe().unwrap();
    let (entered, writing) = mpsc::channel();
    let (release, gate) = mpsc::channel();
    let peer = Peer::connect(
        Side::Host,
        BufReader::new(reader),
        GateSecondFrame {
            pipe: writer,
            first_flush: false,
            gate: Some((entered, gate)),
        },
        Handlers::default(),
    )
    .unwrap();
    let host = peer.handle();
    exited = Observation::spawn(move |probe| {
        probe.observe(|_| peer.wait());
    });
    let written = host.start_request("module.echo", json!(1)).unwrap();
    let failing = host.start_request("module.echo", json!(2)).unwrap();
    writing.recv_timeout(budget()).unwrap();
    let queued = host.start_request("module.echo", json!(3)).unwrap();
    let mut input = BufReader::new(input);
    let Some(Message::Request(received)) = Message::read(&mut input).unwrap() else {
        panic!("expected the request written before stdin closed");
    };
    assert_eq!(received.params, json!(1));
    drop(input);
    let (registered, registration) = mpsc::channel();
    let waiter = Observation::spawn(move |probe| {
        probe.observe(|deadline| {
            queued.wait_registered(deadline.remaining(), || registered.send(()).unwrap())
        });
    });
    let queued_result = waiter.begin(
        Deadline::after(test_timeout(Duration::from_secs(10))),
        "queued waiter after write failure",
    );
    registration.recv_timeout(budget()).unwrap();
    let write_failure = exited.begin(
        Deadline::after(test_timeout(Duration::from_secs(5))),
        "write failure while stdout is open",
    );
    let disconnect_deadline = Deadline::after(test_timeout(Duration::from_secs(1)));
    release.send(()).unwrap();
    let result = queued_result.wait_for(disconnect_deadline).unwrap();
    assert!(matches!(result, Err(CallError::Disconnected)), "{result:?}");
    assert_eq!(
        write_failure.wait().unwrap().unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(matches!(
        failing.wait(budget()),
        Err(CallError::Disconnected)
    ));
    assert!(matches!(
        host.start_request("module.echo", json!(4)),
        Err(CallError::Disconnected)
    ));
    Message::Response(Response::new_ok(received.id, 1))
        .write(&mut output)
        .unwrap();
    assert_eq!(written.wait(budget()).unwrap(), json!(1));
}

#[test]
fn an_unknown_method_cannot_reuse_an_in_flight_request_id() {
    let (reader, writer, mut child) = spawn_module("duplicate-id");
    let mut injection = writer.try_clone().unwrap();
    let (started, starting) = mpsc::channel();
    let mut handlers = Handlers::default();
    handlers.register("host.started", move |_, _, _| {
        started.send(()).unwrap();
        Ok(Value::Null)
    });
    let peer = Peer::connect(Side::Host, reader, writer, handlers).unwrap();
    let request = peer
        .handle()
        .start_request("module.gated", json!(1))
        .unwrap();
    starting.recv_timeout(budget()).unwrap();
    Message::Request(Request::new(
        "h:1".to_owned().into(),
        "module.unknown".into(),
        (),
    ))
    .write(&mut injection)
    .unwrap();
    drop(injection);
    assert!(matches!(
        request.wait(budget()),
        Err(CallError::Disconnected)
    ));
    peer.wait().unwrap();
    let exit = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(5))))
        .unwrap();
    assert!(exit.status.success());
}

#[test]
fn a_panicking_handler_replies_with_internal_error_and_releases_its_id() {
    let (mut reader, writer, child) = spawn_module("peer");
    let responses = Observation::spawn(move |probe| {
        while probe.observe(|_| Message::read(&mut reader).unwrap().unwrap()) {}
    });
    let (mut writer, mut child) = (writer, child);
    let id = RequestId::from("h:1".to_owned());
    for method in ["module.panic", "module.echo"] {
        Message::Request(Request::new(id.clone(), method.into(), 7))
            .write(&mut writer)
            .unwrap();
        let message = responses
            .begin(
                Deadline::after(test_timeout(Duration::from_secs(5))),
                "handler response",
            )
            .wait()
            .unwrap();
        let Message::Response(response) = message else {
            panic!("expected a response, got {message:?}");
        };
        assert_eq!(response.id, id);
        if method == "module.panic" {
            assert_eq!(
                response.response_result.unwrap_err().code,
                ErrorCode::InternalError as i32
            );
        } else {
            assert_eq!(response.response_result.unwrap(), json!(7));
        }
    }
    drop(writer);
    let exit = child
        .wait_until(Deadline::after(test_timeout(Duration::from_secs(5))))
        .unwrap();
    assert!(exit.status.success());
}
