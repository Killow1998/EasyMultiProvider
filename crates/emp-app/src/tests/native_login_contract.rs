use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn browser_login_classifies_engine_failures_and_leaves_http_and_auth_intact() {
    for (program, mode, code, immediate) in [
        (None, 0o700, "login_engine_missing", true),
        (
            Some("#!/bin/sh\nexit 0\n"),
            0o600,
            "login_engine_untrusted",
            true,
        ),
        (
            Some("#!/no/emp-test-interpreter\n"),
            0o700,
            "login_engine_start_failed",
            false,
        ),
        (
            Some("#!/bin/sh\nexit 126\n"),
            0o700,
            "login_engine_start_failed",
            false,
        ),
        (
            Some(
                "#!/bin/sh\nread -r request\nprintf '%s\\n' '{\"id\":1,\"error\":{\"code\":-32601}}'\n",
            ),
            0o700,
            "login_engine_handshake_failed",
            false,
        ),
        (
            Some(
                "#!/bin/sh\nread -r request\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\nread -r request\nread -r request\nprintf '%s\\n' '{\"id\":2,\"error\":{\"code\":-32601}}'\n",
            ),
            0o700,
            "login_browser_unavailable",
            false,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        std::fs::write(&config, b"{}").unwrap();
        let home = root.path().join("codex");
        std::fs::create_dir(&home).unwrap();
        let auth = home.join("auth.json");
        let original = br#"{"tokens":{"account_id":"fixture","access_token":"fixture-only"}}"#;
        emp_state::atomic_write_private_state(&auth, original).unwrap();
        let binary = root.path().join("codex-fixture");
        if let Some(program) = program {
            std::fs::write(&binary, program).unwrap();
            std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let server = ServerHandle::start_with_config_options(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            0,
            &config,
            binary.to_str().unwrap(),
            auth.clone(),
        )
        .unwrap();
        let header = session_header(&server);
        let response = post(
            &server,
            "/api/native/login/start",
            br#"{"method":"browser"}"#,
            &[&header],
        );
        if immediate {
            assert!(
                response.starts_with("HTTP/1.1 409") && response.contains(code),
                "{response}"
            );
        } else {
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let response = post(&server, "/api/native/login/status", b"{}", &[&header]);
                if response.contains("\"status\":\"failed\"") {
                    assert!(response.contains(code), "{response}");
                    break;
                }
                assert!(Instant::now() < deadline, "{code}: {response}");
                #[expect(
                    clippy::disallowed_methods,
                    reason = "假上游以固定间隔重试非阻塞 accept，避免空转"
                )]
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(request(&server, "/healthz", &[]).starts_with("HTTP/1.1 200"));
        assert_eq!(std::fs::read(&auth).unwrap(), original);
        server.shutdown().unwrap();
    }
}
