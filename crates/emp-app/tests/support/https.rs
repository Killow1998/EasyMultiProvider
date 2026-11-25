use emp_test_support::{Deadline, accept_blocking, test_timeout};
use std::io::{self, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, date_time_ymd,
};
use rustls::{
    ServerConfig, ServerConnection, StreamOwned,
    pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
};

use super::{Upstream, http, http::read_message, write_file};

pub struct HttpsProxy {
    port: u16,
    pub certificate: PathBuf,
    connects: Receiver<String>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl HttpsProxy {
    pub fn start(root: &Path, upstream: &Upstream) -> Self {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA parameters");
        ca.not_before = date_time_ymd(2025, 1, 1);
        ca.not_after = date_time_ymd(2030, 1, 1);
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
        ];
        let ca_key = KeyPair::generate().expect("CA key");
        let ca_cert = ca.self_signed(&ca_key).expect("CA certificate");
        let certificate = root.join("upstream-ca.pem");
        write_file(&certificate, &ca_cert.pem());
        let issuer = Issuer::new(ca, ca_key);
        let mut leaf = CertificateParams::new(vec![
            "aiplatform.googleapis.com".to_owned(),
            "us-central1-aiplatform.googleapis.com".to_owned(),
        ])
        .expect("leaf parameters");
        leaf.not_before = date_time_ymd(2025, 1, 1);
        leaf.not_after = date_time_ymd(2030, 1, 1);
        leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        leaf.use_authority_key_identifier_extension = true;
        let leaf_key = KeyPair::generate().expect("leaf key");
        let leaf_cert = leaf
            .signed_by(&leaf_key, &issuer)
            .expect("leaf certificate");
        let tls = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![leaf_cert.der().clone(), ca_cert.der().clone()],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
                )
                .expect("TLS server configuration"),
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind HTTPS proxy");
        let port = listener.local_addr().expect("HTTPS proxy address").port();
        listener.set_nonblocking(true).expect("HTTPS proxy polling");
        let upstream_port = url::Url::parse(&upstream.base_url())
            .expect("upstream URL")
            .port()
            .expect("upstream port");
        let (connected, connects) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Acquire) {
                let (stream, _) = match accept_blocking(&listener) {
                    Ok(accepted) => accepted,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        #[expect(
                            clippy::disallowed_methods,
                            reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                        )]
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("accept HTTPS proxy: {error}"),
                };
                let tls = Arc::clone(&tls);
                let connected = connected.clone();
                thread::spawn(move || {
                    let mut stream =
                        Deadline::after(test_timeout(Duration::from_secs(5))).io(stream);
                    let (connect, _, _) = read_message(&mut stream, false);
                    let authority = connect
                        .strip_prefix("CONNECT ")
                        .and_then(|line| line.strip_suffix(" HTTP/1.1"))
                        .expect("CONNECT request");
                    // Background egress such as the pricing table download is refused.
                    if !matches!(
                        authority,
                        "aiplatform.googleapis.com:443"
                            | "us-central1-aiplatform.googleapis.com:443"
                    ) {
                        let _ = stream
                            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n");
                        return;
                    }
                    connected
                        .send(authority.to_owned())
                        .expect("record CONNECT");
                    stream
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .expect("accept CONNECT");
                    stream.set_deadline(&Deadline::after(test_timeout(Duration::from_secs(5))));
                    let connection = ServerConnection::new(tls).expect("TLS connection");
                    let mut stream = StreamOwned::new(connection, stream);
                    let (request, headers, body) = read_message(&mut stream, false);
                    let mut parts = request.split_whitespace();
                    let method = parts.next().expect("HTTPS method");
                    let path = parts.next().expect("HTTPS path");
                    let headers: Vec<_> = headers
                        .iter()
                        .filter(|(name, _)| {
                            !matches!(name.as_str(), "host" | "connection" | "content-length")
                        })
                        .map(|(name, value)| (name.as_str(), value.as_str()))
                        .collect();
                    let response = http(upstream_port, method, path, &headers, &body);
                    stream
                        .sock
                        .set_deadline(&Deadline::after(test_timeout(Duration::from_secs(5))));
                    write!(stream,
                        "HTTP/1.1 {} Fixture\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        response.status, response.header("content-type").expect("upstream content type"), response.body.len()
                    ).expect("HTTPS response head");
                    stream
                        .write_all(&response.body)
                        .expect("HTTPS response body");
                    stream.conn.send_close_notify();
                    stream.flush().expect("flush HTTPS response");
                });
            }
        });
        Self {
            port,
            certificate,
            connects,
            stop,
            worker: Some(worker),
        }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn next_connect(&self) -> String {
        self.connects
            .recv_timeout(test_timeout(Duration::from_secs(5)).duration())
            .expect("Google request used the loopback proxy")
    }
}

impl Drop for HttpsProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker.join().expect("HTTPS proxy worker");
        }
    }
}
