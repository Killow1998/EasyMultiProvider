use crate::{
    lifecycle::ServerHandle,
    modules::process::ModuleProcess,
    server::{
        http::tests::request,
        raw_tcp::{head, wire_timeout},
    },
};
use emp_modules::{
    handshake::INITIALIZE,
    peer::{Handlers, Peer, Side},
};
use emp_test_support::fixture_dir;
use emp_transport::TimeoutPolicy;
use serde_json::json;
use std::{
    io::{self, BufRead, BufReader, Read},
    net::Ipv4Addr,
    path::Path,
    process::Command,
    time::Instant,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader as AsyncBufReader},
    net::{TcpListener, TcpStream},
    time::timeout,
};

const WORKER_PORT: &str = "EMP_TEST_API3_DISCONNECT_WORKER_PORT";

#[test]
fn client_disconnect_releases_admission_while_waiting_for_module_headers() {
    if let Ok(port) = std::env::var(WORKER_PORT) {
        run_control_worker(port.parse().unwrap());
        return;
    }
    let directory = fixture_dir().unwrap();
    let root = directory.path();
    let server = ServerHandle::start_for_test(
        Ipv4Addr::LOCALHOST.into(),
        &root.join("config.json"),
        root.join("auth.json"),
    )
    .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(check_disconnect(&server, root));
    server.shutdown().unwrap();
}

fn run_control_worker(port: u16) {
    let mut handlers = Handlers::default();
    handlers.register(INITIALIZE, move |_, _, _| {
        Ok(json!({
            "manifest": {
                "id": "disconnect",
                "module_version": "1.0.0",
                "host_api": {"major": 3, "min_minor": 0},
                "contributions": [
                    {"type": "http_route", "id": "wait", "method": "GET", "path": "/wait", "auth": "management"},
                    {"type": "http_route", "id": "upload", "method": "POST", "path": "/wait", "auth": "management"},
                ],
            },
            "data_port": port,
        }))
    });
    Peer::connect(
        Side::Module,
        BufReader::new(io::stdin()),
        io::stdout(),
        handlers,
    )
    .unwrap()
    .wait()
    .unwrap();
}

struct WorkerOutput<R> {
    pipe: R,
    banner_pending: bool,
}

impl<R: BufRead> Read for WorkerOutput<R> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if std::mem::take(&mut self.banner_pending) {
            let mut line = String::new();
            for expected in ["", "running 1 test"] {
                line.clear();
                self.pipe.read_line(&mut line)?;
                if line.trim_end() != expected {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "worker test banner",
                    ));
                }
            }
        }
        self.pipe.read(bytes)
    }
}

async fn check_disconnect(server: &ServerHandle, root: &Path) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tests::module_disconnect_contract::client_disconnect_releases_admission_while_waiting_for_module_headers",
            "--quiet",
            "--nocapture",
        ])
        .env(
            WORKER_PORT,
            listener.local_addr().unwrap().port().to_string(),
        );
    let budget = wire_timeout().duration();
    let (worker, manifest) = ModuleProcess::start_with_connector(
        command,
        root,
        Handlers::default(),
        budget,
        |side, reader, writer, handlers| {
            let reader = BufReader::new(WorkerOutput {
                pipe: reader,
                banner_pending: true,
            });
            Peer::connect(side, reader, writer, handlers)
        },
        || {},
    )
    .await
    .unwrap();
    let address = server.local_addr();
    let backend = &server.state.backend;
    backend
        .modules
        .registry
        .publish(
            manifest,
            1,
            &worker,
            address.port(),
            backend.gateway_key.len(),
        )
        .unwrap();
    let admission = &server.state.lifecycle.admission;
    let session = format!("X-EMP-Session: {}", server.session_token());
    let path = "/api/module-runtime/disconnect/wait";
    for (method, body) in [("GET", b"".as_slice()), ("POST", b"hi".as_slice())] {
        let length = format!("Content-Length: {}", body.len());
        let headers = [&*session, &*length];
        let wire = request(
            address.port(),
            method,
            path,
            if body.is_empty() {
                &headers[..1]
            } else {
                &headers
            },
            body,
        );
        let earliest_response_timeout = Instant::now() + TimeoutPolicy::default().first_event;
        let (client, mut private) = timeout(budget, async {
            let mut client = TcpStream::connect(address).await.unwrap();
            client.write_all(&wire).await.unwrap();
            let mut private = AsyncBufReader::new(listener.accept().await.unwrap().0);
            assert!(
                head(&mut private)
                    .await
                    .unwrap()
                    .starts_with(format!("{method} /wait ").as_bytes())
            );
            let mut received = vec![0; body.len()];
            private.read_exact(&mut received).await.unwrap();
            assert_eq!(received, body);
            (client, private)
        })
        .await
        .unwrap();
        assert_eq!(admission.active_requests(), 1);
        drop(client);
        let remaining = earliest_response_timeout.saturating_duration_since(Instant::now());
        let released = admission.quiesce(0, budget.min(remaining / 2)).is_some();
        assert!(
            released && Instant::now() < earliest_response_timeout,
            "{method} still held its admission slot after the client disconnected"
        );
        assert_eq!(
            timeout(budget, private.read(&mut [0]))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
    timeout(budget, worker.stop()).await.unwrap().unwrap();
}
