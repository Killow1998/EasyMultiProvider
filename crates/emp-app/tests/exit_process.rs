//! Real-process exit checks, with only synthetic configuration and loopback IO.
#[allow(
    dead_code,
    reason = "This target uses only the exit-related subset of shared process fixtures"
)]
mod support;

#[cfg(windows)]
#[path = "exit_process/windows.rs"]
mod windows;

use emp_test_support::{Deadline, test_timeout};
use std::time::Duration;
use support::{Emp, ORIGINAL_CODEX_CONFIG, Upstream, Workspace};

fn assert_native(workspace: &Workspace) {
    let lines = |text: &str| {
        text.lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        lines(&workspace.read_codex_config()),
        lines(ORIGINAL_CODEX_CONFIG)
    );
    let lease: serde_json::Value = serde_json::from_slice(
        &std::fs::read(workspace.integration_dir().join("lease.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(lease["status"], "restored");
}

fn await_native(workspace: &Workspace) {
    let deadline = Deadline::after(test_timeout(Duration::from_secs(2)));
    loop {
        let lease: serde_json::Value = serde_json::from_slice(
            &std::fs::read(workspace.integration_dir().join("lease.json")).unwrap(),
        )
        .unwrap();
        if lease["status"] == "restored" {
            deadline.accept((), "native routing restored").unwrap();
            break;
        }
        assert!(
            !deadline.is_expired(),
            "native routing was not restored before the exit deadline"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "退出恢复结果写入配置文件，以固定间隔轮询"
        )]
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_native(workspace);
}

fn enable(emp: &Emp, workspace: &Workspace) {
    let response = emp.post(
        "/api/integration/enable",
        &serde_json::json!({"confirm_reload":true}),
    );
    assert_eq!(response.status, 200, "{}", response.text());
    assert!(workspace.read_codex_config().contains(&format!(
        "http://{}:{}/v1",
        if cfg!(windows) {
            "localhost"
        } else {
            "127.0.0.1"
        },
        emp.port
    )));
}

#[cfg(unix)]
#[test]
fn terminal_signals_restore_native_with_an_unfinished_request() {
    use std::io::Write;
    use std::net::TcpStream;

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let upstream = Upstream::start();
        let workspace = Workspace::with_routes(&upstream);
        let mut emp = workspace.start();
        enable(&emp, &workspace);
        let mut pending = TcpStream::connect(("127.0.0.1", emp.port)).unwrap();
        write!(
            pending,
            "POST /v1/responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100000\r\n\r\n{{"
        )
        .unwrap();
        emp.signal(signal);
        await_native(&workspace);
        assert_eq!(emp.wait_exit(), Some(0));
        drop(pending);
        upstream.assert_idle();
    }
}

#[cfg(unix)]
#[test]
fn exit_restores_while_runtime_observation_is_stalled() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::os::unix::net::UnixListener;

    let upstream = Upstream::start();
    let workspace = Workspace::with_routes_in(&upstream, "/tmp");
    let mut emp = workspace.start();
    enable(&emp, &workspace);
    let control = workspace.codex_home.join("app-server-control");
    std::fs::create_dir_all(&control).unwrap();
    let listener = UnixListener::bind(control.join("app-server-control.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut observation = TcpStream::connect(("127.0.0.1", emp.port)).unwrap();
    write!(observation, "POST /api/integration/verify HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-EMP-Session: {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}", emp.port, emp.session).unwrap();
    let deadline = Deadline::after(test_timeout(Duration::from_secs(3)));
    let mut socket = loop {
        if let Ok((socket, _)) = listener.accept() {
            break deadline
                .accept(socket, "runtime observation started")
                .unwrap();
        }
        assert!(
            !deadline.is_expired(),
            "EMP never started the runtime observation"
        );
        #[expect(
            clippy::disallowed_methods,
            reason = "运行时观测通过管理接口提供，以固定间隔轮询"
        )]
        std::thread::sleep(Duration::from_millis(10));
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut head = [0; 1024];
    assert!(socket.read(&mut head).unwrap() > 0);
    // Leave the real runtime probe blocked in its WebSocket handshake.
    emp.signal(libc::SIGTERM);
    await_native(&workspace);
    // Native settings must be restored while IPC is still blocked. Release
    // the fixture only afterwards so normal exit can drain the handler.
    drop(socket);
    drop(listener);
    assert_eq!(emp.wait_exit(), Some(0));
    drop(observation);
}
