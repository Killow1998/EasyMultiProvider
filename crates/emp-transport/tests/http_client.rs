use emp_transport::{
    HttpClient, HttpClientConfig, HttpClientPolicy, HttpMethod, HttpTransportErrorKind,
    ProxyPolicy, TimeoutPolicy,
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, date_time_ymd,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::{ServerConfig, ServerConnection, StreamOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::{path::Path, process::Command};

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedRequest {
    connection: u64,
    client_port: u16,
    method: String,
    target: String,
    authorization: Option<String>,
    proxy_authorization: Option<String>,
    accept_encoding: Option<String>,
    oracle_lane: Option<String>,
}

struct TestServer {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    release_streams: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind test server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let address = listener.local_addr().expect("test server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let release_streams = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let next_connection = Arc::new(AtomicU64::new(1));
        let accept_thread = {
            let requests = Arc::clone(&requests);
            let release_streams = Arc::clone(&release_streams);
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, peer)) => {
                            let connection = next_connection.fetch_add(1, Ordering::Relaxed);
                            let requests = Arc::clone(&requests);
                            let release_streams = Arc::clone(&release_streams);
                            let shutdown = Arc::clone(&shutdown);
                            thread::spawn(move || {
                                serve_connection(
                                    stream,
                                    peer,
                                    connection,
                                    requests,
                                    release_streams,
                                    shutdown,
                                );
                            });
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            address,
            requests,
            release_streams,
            shutdown,
            accept_thread: Some(accept_thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}:{}{path}", self.address.ip(), self.address.port())
    }

    fn proxy_url(&self, userinfo: &str) -> String {
        format!(
            "http://{userinfo}@{}:{}",
            self.address.ip(),
            self.address.port()
        )
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn release_streams(&self) {
        self.release_streams.store(true, Ordering::Release);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.release_streams();
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.accept_thread.take() {
            thread.join().expect("join test server");
        }
    }
}

struct TlsTestServer {
    address: SocketAddr,
    certificate_der: Vec<u8>,
    shutdown: Arc<AtomicBool>,
    accept_thread: Option<thread::JoinHandle<()>>,
}

impl TlsTestServer {
    fn start() -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Windows and macOS platform verifiers reject a self-signed leaf even
        // when it is supplied as an extra root. Use a real test CA and a
        // separately signed server leaf so every platform exercises the same
        // trust-chain and hostname checks.
        let mut ca_params = CertificateParams::new(Vec::<String>::new())
            .expect("empty CA subject alternative names");
        ca_params.not_before = date_time_ymd(2025, 1, 1);
        ca_params.not_after = date_time_ymd(2030, 1, 1);
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "EMP test root");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().expect("generate TLS CA key");
        let ca_certificate = ca_params.self_signed(&ca_key).expect("generate TLS CA");
        let issuer = Issuer::new(ca_params, ca_key);

        let mut leaf_params =
            CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                .expect("loopback TLS subject alternative names");
        leaf_params.not_before = date_time_ymd(2025, 1, 1);
        leaf_params.not_after = date_time_ymd(2030, 1, 1);
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params.use_authority_key_identifier_extension = true;
        let leaf_key = KeyPair::generate().expect("generate TLS leaf key");
        let leaf_certificate = leaf_params
            .signed_by(&leaf_key, &issuer)
            .expect("sign TLS leaf");

        let certificate_der = ca_certificate.der().to_vec();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let server_config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                // Send the complete generated chain so every verifier receives
                // identical issuer material from this synthetic fixture.
                .with_single_cert(
                    vec![leaf_certificate.der().clone(), ca_certificate.der().clone()],
                    private_key,
                )
                .expect("TLS server configuration"),
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind TLS server");
        listener
            .set_nonblocking(true)
            .expect("nonblocking TLS listener");
        let address = listener.local_addr().expect("TLS server address");
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept_thread = {
            let shutdown = Arc::clone(&shutdown);
            thread::spawn(move || {
                while !shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            // Exercise the accepted-socket mode seen on macOS
                            // and Windows even when Linux defaults to blocking.
                            stream
                                .set_nonblocking(true)
                                .expect("nonblocking accepted TLS stream");
                            let config = Arc::clone(&server_config);
                            thread::spawn(move || serve_tls_connection(stream, config));
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(_) => break,
                    }
                }
            })
        };
        Self {
            address,
            certificate_der,
            shutdown,
            accept_thread: Some(accept_thread),
        }
    }

    fn url(&self, host: &str) -> String {
        format!("https://{host}:{}/", self.address.port())
    }
}

impl Drop for TlsTestServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(thread) = self.accept_thread.take() {
            thread.join().expect("join TLS server");
        }
    }
}

fn serve_tls_connection(stream: TcpStream, config: Arc<ServerConfig>) {
    // The listener is nonblocking, and accepted sockets can inherit that mode.
    // The TLS fixture uses blocking reads and writes on its own worker thread.
    stream
        .set_nonblocking(false)
        .expect("blocking accepted TLS stream");
    let connection = ServerConnection::new(config).expect("TLS server connection");
    let mut stream = BufReader::new(StreamOwned::new(connection, stream));
    let mut request_line = String::new();
    match stream.read_line(&mut request_line) {
        Ok(0) => return,
        Err(error) => {
            eprintln!("TLS fixture could not read request: {error:?}");
            return;
        }
        Ok(_) => {}
    }
    let mut content_length = 0;
    loop {
        let mut header = String::new();
        match stream.read_line(&mut header) {
            Ok(0) | Err(_) => return,
            Ok(_) if header == "\r\n" => break,
            Ok(_) => {
                if let Some((name, value)) = header.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().unwrap_or(0);
                }
            }
        }
    }
    let mut body = vec![0_u8; content_length];
    if stream.read_exact(&mut body).is_err() {
        return;
    }
    let stream = stream.get_mut();
    if stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .is_err()
    {
        return;
    }
    stream.conn.send_close_notify();
    let _ = stream.flush();
}

fn serve_connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    connection: u64,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    release_streams: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .expect("request timeout");
    loop {
        let Some((head, _body)) = read_request(&mut stream, &shutdown) else {
            return;
        };
        let text = String::from_utf8(head).expect("HTTP request is ASCII");
        let mut lines = text.split("\r\n");
        let mut request_line = lines.next().expect("request line").split_whitespace();
        let method = request_line.next().unwrap_or_default().to_owned();
        let target = request_line.next().unwrap_or_default().to_owned();
        let mut authorization = None;
        let mut proxy_authorization = None;
        let mut accept_encoding = None;
        let mut oracle_lane = None;
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => authorization = Some(value.trim().to_owned()),
                "proxy-authorization" => proxy_authorization = Some(value.trim().to_owned()),
                "accept-encoding" => accept_encoding = Some(value.trim().to_owned()),
                "x-oracle-lane" => oracle_lane = Some(value.trim().to_owned()),
                _ => {}
            }
        }
        requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(RecordedRequest {
                connection,
                client_port: peer.port(),
                method,
                target: target.clone(),
                authorization,
                proxy_authorization,
                accept_encoding,
                oracle_lane,
            });

        if target.ends_with("/drop") {
            return;
        }
        if target.ends_with("/redirect") {
            if stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: /unexpected\r\nContent-Length: 0\r\n\r\n",
                )
                .is_err()
            {
                return;
            }
            continue;
        }
        if target.ends_with("/status-307") {
            let body = b"retry elsewhere";
            if stream
                .write_all(
                    format!(
                        "HTTP/1.1 307 Temporary Redirect\r\nLocation: /voice-temporarily-unavailable\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .and_then(|_| stream.write_all(body))
                .is_err()
            {
                return;
            }
            continue;
        }
        if target.ends_with("/stream") || target.ends_with("/stall") {
            if stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n")
                .is_err()
            {
                return;
            }
            if target.ends_with("/stream") && stream.write_all(b"data: first\n\n").is_err() {
                return;
            }
            let _ = stream.flush();
            while !release_streams.load(Ordering::Acquire) && !shutdown.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(2));
            }
            let _ = stream.write_all(&vec![b'x'; 100_000 - 13]);
            let _ = stream.flush();
            return;
        }
        if stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
            .is_err()
        {
            return;
        }
        let _ = stream.flush();
    }
}

fn read_request(stream: &mut TcpStream, shutdown: &AtomicBool) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut received = Vec::new();
    let header_end = loop {
        if let Some(position) = received.windows(4).position(|part| part == b"\r\n\r\n") {
            break position + 4;
        }
        let mut buffer = [0_u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(count) => received.extend_from_slice(&buffer[..count]),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if shutdown.load(Ordering::Acquire) {
                    return None;
                }
            }
            Err(_) => return None,
        }
    };
    let content_length = String::from_utf8_lossy(&received[..header_end])
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while received.len() < header_end + content_length {
        let mut buffer = [0_u8; 4096];
        match stream.read(&mut buffer) {
            Ok(0) => return None,
            Ok(count) => received.extend_from_slice(&buffer[..count]),
            Err(_) => return None,
        }
    }
    let body = received[header_end..header_end + content_length].to_vec();
    received.truncate(header_end);
    Some((received, body))
}

fn headers(authorization: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("Authorization".to_owned(), authorization.to_owned())])
}

fn oracle_headers(lane: &str, authorization: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("Authorization".to_owned(), authorization.to_owned()),
        ("X-Oracle-Lane".to_owned(), lane.to_owned()),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_responses_reuse_connections_without_sharing_authorization() {
    let server = TestServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    for authorization in ["Bearer first", "Bearer second"] {
        let response = client
            .open(
                HttpMethod::Get,
                &server.url("/complete"),
                headers(authorization),
                None,
                false,
            )
            .await
            .expect("complete response");
        assert_eq!(response.status(), 200);
        assert_eq!(response.read_all().await.expect("complete body"), b"OK");
    }
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].connection, requests[1].connection);
    assert_eq!(requests[0].client_port, requests[1].client_port);
    assert_eq!(
        requests
            .iter()
            .map(|request| request.authorization.as_deref())
            .collect::<Vec<_>>(),
        [Some("Bearer first"), Some("Bearer second")]
    );
    assert!(
        requests
            .iter()
            .all(|request| request.accept_encoding.as_deref() == Some("identity"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirects_and_dropped_posts_are_never_replayed() {
    let server = TestServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let dropped = client
        .open(
            HttpMethod::Post,
            &server.url("/drop"),
            BTreeMap::new(),
            Some(b"one request".to_vec()),
            false,
        )
        .await
        .expect_err("dropped request must fail");
    assert_eq!(dropped.kind(), HttpTransportErrorKind::Network);
    let redirect = client
        .open(
            HttpMethod::Get,
            &server.url("/redirect"),
            headers("Bearer private"),
            None,
            false,
        )
        .await
        .expect_err("redirect must fail");
    assert_eq!(redirect.kind(), HttpTransportErrorKind::RedirectDisabled);
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.target.as_str())
            .collect::<Vec<_>>(),
        ["/drop", "/redirect"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_status_preserves_redirect_response_without_following() {
    let server = TestServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let response = client
        .open_status(
            HttpMethod::Post,
            &server.url("/status-307"),
            BTreeMap::new(),
            Some(b"offer".to_vec()),
            false,
        )
        .await
        .expect("307 is an upstream response, not a redirect instruction");
    assert_eq!(response.status(), 307);
    assert_eq!(
        response.header("location"),
        Some("/voice-temporarily-unavailable")
    );
    assert_eq!(response.header("content-type"), Some("text/plain"));
    assert_eq!(response.read_all().await.unwrap(), b"retry elsewhere");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/status-307");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streaming_arrives_before_eof_and_cancellation_discards_the_connection() {
    let server = TestServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let mut response = client
        .open(
            HttpMethod::Get,
            &server.url("/stream"),
            BTreeMap::new(),
            None,
            true,
        )
        .await
        .expect("stream response");
    assert_eq!(
        response.next_chunk().await.expect("first stream chunk"),
        Some(b"data: first\n\n".to_vec())
    );
    drop(response);
    let complete = client
        .open(
            HttpMethod::Get,
            &server.url("/complete"),
            BTreeMap::new(),
            None,
            false,
        )
        .await
        .expect("request after cancellation");
    assert_eq!(complete.read_all().await.expect("complete body"), b"OK");
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert_ne!(requests[0].connection, requests[1].connection);
    server.release_streams();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_event_timeout_is_reported_as_a_read_timeout() {
    let server = TestServer::start();
    let timeout = TimeoutPolicy {
        connect: Duration::from_secs(1),
        first_event: Duration::from_millis(40),
        stream_idle: Duration::from_millis(40),
        non_stream_wall_clock: Duration::from_secs(1),
        cleanup: Duration::from_millis(20),
    };
    let client = HttpClient::new(HttpClientPolicy::new(ProxyPolicy::default(), timeout))
        .expect("HTTP client");
    let mut response = client
        .open(
            HttpMethod::Get,
            &server.url("/stall"),
            BTreeMap::new(),
            None,
            true,
        )
        .await
        .expect("stream headers");
    let error = response
        .next_chunk()
        .await
        .expect_err("first event must time out");
    assert_eq!(error.kind(), HttpTransportErrorKind::ReadTimeout);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_stream_body_limit_rejects_before_buffering_the_declared_body() {
    let server = TestServer::start();
    let client = HttpClient::new(HttpClientPolicy::default()).expect("HTTP client");
    let response = client
        .open(
            HttpMethod::Get,
            &server.url("/complete"),
            BTreeMap::new(),
            None,
            false,
        )
        .await
        .expect("complete response");
    let error = response
        .read_limited(1)
        .await
        .expect_err("declared body exceeds limit");
    assert_eq!(error.kind(), HttpTransportErrorKind::ResponseTooLarge);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_credentials_stay_separate_from_origin_authorization() {
    let proxy = TestServer::start();
    let policy = HttpClientPolicy::new(
        ProxyPolicy::explicit(Some(proxy.proxy_url("user%40test:pass%3Aword"))),
        TimeoutPolicy::default(),
    );
    let client = HttpClient::new(policy).expect("proxied HTTP client");
    for _ in 0..2 {
        let response = client
            .open(
                HttpMethod::Get,
                "http://origin.test/resource",
                headers("Bearer origin-only"),
                None,
                false,
            )
            .await
            .expect("proxy response");
        assert_eq!(response.read_all().await.expect("proxy body"), b"OK");
    }
    let requests = proxy.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].connection, requests[1].connection);
    assert!(
        requests
            .iter()
            .all(|request| request.authorization.as_deref() == Some("Bearer origin-only"))
    );
    assert!(requests.iter().all(|request| {
        request.proxy_authorization.as_deref() == Some("Basic dXNlckB0ZXN0OnBhc3M6d29yZA==")
    }));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tls_rejects_untrusted_and_wrong_host_but_accepts_a_trusted_name() {
    let server = TlsTestServer::start();
    let untrusted =
        HttpClient::with_config(HttpClientPolicy::default(), HttpClientConfig::default())
            .expect("untrusted client");
    let error = untrusted
        .open(
            HttpMethod::Get,
            &server.url("127.0.0.1"),
            BTreeMap::new(),
            None,
            false,
        )
        .await
        .expect_err("untrusted certificate must fail");
    assert_eq!(error.kind(), HttpTransportErrorKind::Network);

    let mut config = HttpClientConfig::default();
    config
        .add_root_certificate_der(&server.certificate_der)
        .expect("test root certificate");
    config
        .use_only_configured_root_certificates()
        .expect("hermetic test trust store");
    config
        .add_dns_override("wrong.test", server.address)
        .expect("wrong-host test DNS override");
    let trusted =
        HttpClient::with_config(HttpClientPolicy::default(), config).expect("trusted client");
    let response = trusted
        .open(
            HttpMethod::Get,
            &server.url("127.0.0.1"),
            BTreeMap::new(),
            None,
            false,
        )
        .await
        .expect("trusted TLS request");
    assert_eq!(response.status(), 200);
    assert_eq!(response.header("content-length"), Some("2"));
    response.finish().await;

    let wrong_host = trusted
        .open(
            HttpMethod::Get,
            &server.url("wrong.test"),
            BTreeMap::new(),
            None,
            false,
        )
        .await
        .expect_err("wrong TLS host must fail");
    assert_eq!(wrong_host.kind(), HttpTransportErrorKind::Network);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_behavior_matches_the_live_python_oracle_when_configured() {
    let Ok(python) = std::env::var("EMP_PYTHON_INTEROP") else {
        return;
    };
    let server = TestServer::start();
    let script = r#"
import json, sys
from urllib.error import URLError
from urllib.request import Request
from easy_multi_provider import http_pool

base = sys.argv[1]
success = []
http_pool.close_pools()
for token in ("Bearer first", "Bearer second"):
    request = Request(base + "/complete", headers={"Authorization": token, "X-Oracle-Lane": "python"})
    with http_pool.open_request(request, timeout=2) as response:
        success.append({"status": response.status, "body": response.read().decode(), "terminal": "eof"})
stream = http_pool.open_request(Request(base + "/stream", headers={"X-Oracle-Lane": "python"}), timeout=2)
first_line = next(stream).decode()
stream.close()
with http_pool.open_request(Request(base + "/complete", headers={"X-Oracle-Lane": "python"}), timeout=2) as response:
    after_cancel = {"status": response.status, "body": response.read().decode(), "terminal": "eof"}
try:
    http_pool.open_request(Request(base + "/redirect", headers={"X-Oracle-Lane": "python"}), timeout=2)
except Exception as exc:
    redirect = {
        "native_type": type(exc).__name__, "class": "redirect_disabled",
        "http_status": None, "terminal": "error", "retry": False,
    }
else:
    raise AssertionError("redirect unexpectedly followed")
json.dump({"success": success, "first_line": first_line, "after_cancel": after_cancel,
           "redirect": redirect}, sys.stdout, separators=(",", ":"))
"#;
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(python)
        .arg("-c")
        .arg(script)
        .arg(server.url(""))
        .current_dir(root)
        .output()
        .expect("spawn Python HTTP oracle");
    assert!(
        output.status.success(),
        "Python HTTP oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut oracle: Value = serde_json::from_slice(&output.stdout).expect("Python oracle JSON");
    assert_eq!(oracle["redirect"]["native_type"], "URLError");
    oracle["redirect"]
        .as_object_mut()
        .expect("redirect object")
        .remove("native_type");

    let client = HttpClient::new(HttpClientPolicy::default()).expect("Rust HTTP client");
    let mut success = Vec::new();
    for token in ["Bearer first", "Bearer second"] {
        let response = client
            .open(
                HttpMethod::Get,
                &server.url("/complete"),
                oracle_headers("rust", token),
                None,
                false,
            )
            .await
            .expect("Rust complete response");
        let status = response.status();
        let body = String::from_utf8(response.read_all().await.expect("Rust complete body"))
            .expect("UTF-8 body");
        success.push(json!({"status": status, "body": body, "terminal": "eof"}));
    }
    let mut stream = client
        .open(
            HttpMethod::Get,
            &server.url("/stream"),
            BTreeMap::from([("X-Oracle-Lane".to_owned(), "rust".to_owned())]),
            None,
            true,
        )
        .await
        .expect("Rust stream response");
    let mut first_line = Vec::new();
    while !first_line.contains(&b'\n') {
        first_line.extend(
            stream
                .next_chunk()
                .await
                .expect("Rust stream chunk")
                .expect("Rust stream data"),
        );
    }
    first_line.truncate(
        first_line
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("newline")
            + 1,
    );
    drop(stream);
    let after_cancel_response = client
        .open(
            HttpMethod::Get,
            &server.url("/complete"),
            BTreeMap::from([("X-Oracle-Lane".to_owned(), "rust".to_owned())]),
            None,
            false,
        )
        .await
        .expect("Rust request after cancellation");
    let after_cancel_status = after_cancel_response.status();
    let after_cancel_body = String::from_utf8(
        after_cancel_response
            .read_all()
            .await
            .expect("Rust body after cancellation"),
    )
    .expect("UTF-8 body");
    let redirect = client
        .open(
            HttpMethod::Get,
            &server.url("/redirect"),
            BTreeMap::from([("X-Oracle-Lane".to_owned(), "rust".to_owned())]),
            None,
            false,
        )
        .await
        .expect_err("Rust redirect disabled");
    assert_eq!(redirect.kind(), HttpTransportErrorKind::RedirectDisabled);
    let rust = json!({
        "success": success,
        "first_line": String::from_utf8(first_line).expect("UTF-8 SSE line"),
        "after_cancel": {
            "status": after_cancel_status, "body": after_cancel_body, "terminal": "eof"
        },
        "redirect": {
            "class": "redirect_disabled", "http_status": null,
            "terminal": "error", "retry": false,
        },
    });
    assert_eq!(rust, oracle);

    let requests = server.requests();
    for lane in ["python", "rust"] {
        let requests = requests
            .iter()
            .filter(|request| request.oracle_lane.as_deref() == Some(lane))
            .collect::<Vec<_>>();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[0].connection, requests[1].connection);
        assert_ne!(requests[2].connection, requests[3].connection);
        assert_eq!(requests[4].target, "/redirect");
    }
    server.release_streams();
}
