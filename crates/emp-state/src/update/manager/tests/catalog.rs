//! Cache persistence and HTTP validators through the real updater client.
use super::*;

fn release(base: &str, notes: &str) -> String {
    let name = crate::update::release::current_asset_name().unwrap();
    serde_json::json!({
        "tag_name":"v9.0.0", "draft":false, "prerelease":false, "body":notes,
        "assets":[{"name":name,"digest":format!("sha256:{}","a".repeat(64)),"size":8,
            "browser_download_url":format!("{base}/releases/download/v9.0.0/{name}")}]
    })
    .to_string()
}

fn manager(root: &std::path::Path, base: &str, version: &str) -> UpdateManager {
    UpdateManager::with_endpoints(
        std::env::current_exe().unwrap(),
        vec![
            "--config".into(),
            root.join("config.json").to_string_lossy().into_owned(),
        ],
        version,
        UpdateEndpoints::for_source(base, format!("{base}/latest")),
        || panic!("release cache checks must not install or exit"),
    )
    .unwrap()
}

#[test]
fn cached_release_survives_restart_revalidation_edits_and_failed_checks() {
    let root = tempfile::TempDir::new().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let responses = vec![
        (200, "\"first\"", release(&base, "Original notes"), None),
        (304, "\"first\"", String::new(), Some("\"first\"")),
        (
            200,
            "W/\"edited\"",
            release(&base, "Edited same-version notes"),
            Some("\"first\""),
        ),
        (403, "", String::new(), Some("W/\"edited\"")),
        (200, "\"malformed\"", "{broken".into(), Some("W/\"edited\"")),
        (200, "\"invalid\"", "{}".into(), Some("W/\"edited\"")),
        (404, "", String::new(), Some("W/\"edited\"")),
    ];
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        for (status, etag, body, expected) in responses {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing release check");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0);
                request.extend_from_slice(&buffer[..count]);
            }
            let request = String::from_utf8(request).unwrap();
            let validator = request.lines().find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("if-none-match")
                    .then(|| value.trim())
            });
            assert_eq!(validator, expected);
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    manager(root.path(), &base, "0.13.7").check().unwrap();
    let cache_path = root.path().join("state/update-release.json");
    let original_cache = std::fs::read(&cache_path).unwrap();
    let restarted = manager(root.path(), &base, "0.13.7");
    assert_eq!(restarted.snapshot().release_notes, "Original notes");
    assert_eq!(restarted.snapshot().state, "idle");
    assert!(
        restarted.start("install").is_err(),
        "disk cache alone cannot authorize an install"
    );
    restarted.check().unwrap();
    assert_eq!(restarted.snapshot().state, "available");
    assert_eq!(std::fs::read(&cache_path).unwrap(), original_cache);
    restarted.check().unwrap();
    assert_eq!(
        restarted.snapshot().release_notes,
        "Edited same-version notes"
    );
    let edited_cache = std::fs::read(&cache_path).unwrap();
    for expected in ["check_failed", "update_failed", "invalid_release"] {
        assert_eq!(
            restarted.check().unwrap_err(),
            crate::update::UpdateError(expected)
        );
        assert_eq!(
            restarted.snapshot().release_notes,
            "Edited same-version notes"
        );
        assert_eq!(std::fs::read(&cache_path).unwrap(), edited_cache);
    }
    let upgraded = manager(root.path(), &base, "9.0.0");
    assert_eq!(upgraded.snapshot().current_version, "9.0.0");
    assert_eq!(
        upgraded.snapshot().release_notes,
        "Edited same-version notes"
    );
    assert!(
        manager(root.path(), "http://127.0.0.1:1", "0.13.7")
            .snapshot()
            .release_notes
            .is_empty()
    );
    restarted.check().unwrap();
    assert_eq!(restarted.snapshot().state, "no_release");
    assert!(restarted.snapshot().release_notes.is_empty());
    assert!(!cache_path.exists());
    server.join().unwrap();
    std::fs::write(&cache_path, b"{broken").unwrap();
    assert!(
        manager(root.path(), &base, "0.13.7")
            .snapshot()
            .release_notes
            .is_empty()
    );
}

#[test]
fn not_modified_without_cached_metadata_is_rejected() {
    let root = tempfile::TempDir::new().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        retry::serve_responses(
            listener,
            vec![b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()],
        )
    });
    let manager = manager(root.path(), &base, "0.13.7");
    assert_eq!(
        manager.check().unwrap_err(),
        crate::update::UpdateError("invalid_release")
    );
    assert!(manager.snapshot().release_notes.is_empty());
    assert_eq!(server.join().unwrap(), 1);
}
