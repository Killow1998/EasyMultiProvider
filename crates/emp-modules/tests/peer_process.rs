#![cfg(unix)]

use emp_modules::peer::{CallError, ErrorCode, Handlers, Peer, Side};
use emp_test_support::{OwnedChild, test_timeout};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{self, BufReader, Write},
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

const CHILD_MODE: &str = "EMP_TEST_API3_PEER";

fn budget() -> Duration {
    test_timeout(Duration::from_secs(5)).duration()
}

fn module(mode: &str, handlers: Handlers) -> (Peer, OwnedChild) {
    let (reader, stdout) = io::pipe().unwrap();
    let mut command = Command::new("sh");
    command
        .args(["-c", r#"exec "$@" 3>&1 1>&2"#, "sh"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "module_fixture", "--nocapture"])
        .env(CHILD_MODE, mode)
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::null());
    let mut child = OwnedChild::spawn(command).unwrap();
    let peer = Peer::connect(
        Side::Host,
        BufReader::new(reader),
        child.take_stdin().unwrap(),
        handlers,
    )
    .unwrap();
    (peer, child)
}

#[test]
fn module_fixture() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let mut output = OpenOptions::new().write(true).open("/dev/fd/3").unwrap();
    let mut input = BufReader::new(io::stdin());
    if mode == "oversized" || mode == "wrong-prefix" {
        let request = lsp_server::Message::read(&mut input).unwrap().unwrap();
        assert!(
            matches!(request, lsp_server::Message::Request(req) if req.id == "h:1".to_owned().into())
        );
        if mode == "oversized" {
            output
                .write_all(b"Content-Length: 16777217\r\n\r\n")
                .unwrap();
            let _ = lsp_server::Message::read(&mut input);
        } else {
            lsp_server::Message::Response(lsp_server::Response::new_ok(
                "m:1".to_owned().into(),
                true,
            ))
            .write(&mut output)
            .unwrap();
            let _ = lsp_server::Message::read(&mut input);
        }
        return;
    }
    let mut handlers = Handlers::default();
    handlers.register("module.echo", |params, _, _| Ok(params));
    handlers.register("module.callback", |params, token, peer| {
        let reply = peer.call("host.echo", params, budget()).unwrap();
        peer.notify("host.callback_done", json!([reply, token.is_cancelled()]))
            .unwrap();
        Ok(reply)
    });
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
    Peer::connect(Side::Module, input, output, handlers)
        .unwrap()
        .wait()
        .unwrap();
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
    let (peer, _child) = module("peer", handlers);
    let host = peer.handle();
    let request = host.start_request("module.gated", json!(1)).unwrap();
    assert_eq!(starting.recv_timeout(budget()).unwrap(), json!(1));
    request.canceller().cancel();
    assert!(matches!(request.wait(budget()), Err(CallError::Cancelled)));
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
    handlers.register("host.callback_done", move |params, _, _| {
        finished.send(params).unwrap();
        Ok(Value::Null)
    });
    let (peer, _child) = module("peer", handlers);
    let host = peer.handle();
    let parent = host.start_request("module.callback", json!(5)).unwrap();
    starting.recv_timeout(budget()).unwrap();
    parent.canceller().cancel();
    assert!(matches!(parent.wait(budget()), Err(CallError::Cancelled)));
    host.call("module.echo", json!(null), budget()).unwrap();
    release.send(()).unwrap();
    assert_eq!(finishing.recv_timeout(budget()).unwrap(), json!([5, true]));
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
fn invalid_response_prefix_and_oversized_frames_disconnect() {
    for mode in ["wrong-prefix", "oversized"] {
        let (peer, _child) = module(mode, Handlers::default());
        let host = peer.handle();
        assert!(
            matches!(
                host.call("module.echo", Value::Null, budget()),
                Err(CallError::Disconnected)
            ),
            "{mode}"
        );
    }
}
