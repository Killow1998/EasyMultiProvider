//! Real HTTP failures exercise the same retry path as the packaged updater.
use super::*;
use crate::update::{UpdateError, download::download_package, release::Asset};
use sha2::{Digest, Sha256};

pub(super) fn serve_responses(listener: TcpListener, responses: Vec<Vec<u8>>) -> usize {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut requests = 0;
    for response in responses {
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "missing updater request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fake release server: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|chunk| chunk == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        stream.write_all(&response).unwrap();
        requests += 1;
    }
    requests
}

fn fixture(responses: Vec<Vec<u8>>) -> (UpdateManager, Asset, thread::JoinHandle<usize>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || serve_responses(listener, responses));
    let manager = UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        Vec::new(),
        env!("CARGO_PKG_VERSION"),
        UpdateEndpoints::for_source(&base, format!("{base}/latest")),
        || panic!("network tests must not hand off or exit EMP"),
    )
    .unwrap();
    let asset = Asset {
        version: "9.0.0".into(),
        name: "fixture".into(),
        url: format!("{base}/package"),
        size: 8,
        digest: format!("{:x}", Sha256::digest(b"complete")),
    };
    (manager, asset, server)
}

#[test]
fn interrupted_download_retries_from_zero_and_verifies_the_complete_file() {
    // Both a transport-level truncated body and a short close-delimited body
    // must discard the first attempt's bytes and digest.
    for first in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\npart".to_vec(),
        b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\npart".to_vec(),
    ] {
        let (manager, asset, server) = fixture(vec![
            first,
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncomplete".to_vec(),
        ]);
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("package");
        download_package(&manager, &asset, &destination, |_| {}).unwrap();
        assert_eq!(server.join().unwrap(), 2);
        assert_eq!(std::fs::read(destination).unwrap(), b"complete");
        assert_eq!(manager.snapshot().retry_count, 1);
    }
}

#[test]
fn permanent_download_failures_do_not_retry() {
    for (response, expected) in [
        (
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
            "update_failed",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncorrupt!".to_vec(),
            "checksum_mismatch",
        ),
    ] {
        let (manager, asset, server) = fixture(vec![response]);
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            download_package(&manager, &asset, &root.path().join("package"), |_| {}).unwrap_err(),
            UpdateError(expected)
        );
        assert_eq!(server.join().unwrap(), 1);
        assert_eq!(manager.snapshot().retry_count, 0);
    }
}

#[test]
fn release_check_recovers_from_a_temporary_service_failure() {
    let (manager, _, server) = fixture(vec![
        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec(),
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    ]);
    manager.check().unwrap();
    assert_eq!(server.join().unwrap(), 2);
    assert_eq!(manager.snapshot().state, "no_release");
    assert_eq!(manager.snapshot().retry_count, 1);
}
