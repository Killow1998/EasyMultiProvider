//! Authenticated, path-only Claude CLI availability endpoint checks.
use super::*;

fn response_json(response: &str) -> Value {
    serde_json::from_str(
        response
            .split_once("\r\n\r\n")
            .expect("HTTP response separator")
            .1,
    )
    .expect("availability response JSON")
}

#[test]
fn claude_cli_availability_requires_a_same_origin_management_session() {
    let (_directory, server) = test_server();

    let unauthenticated = request(&server, "/api/runtime/claude-cli", &[]);
    assert!(
        unauthenticated.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "{unauthenticated}"
    );

    let cross_origin = request(
        &server,
        "/api/runtime/claude-cli",
        &["Host: attacker.invalid", &session_header(&server)],
    );
    assert!(
        cross_origin.starts_with("HTTP/1.1 403 Forbidden\r\n"),
        "{cross_origin}"
    );

    let authenticated = request(
        &server,
        "/api/runtime/claude-cli",
        &[&session_header(&server)],
    );
    assert!(
        authenticated.starts_with("HTTP/1.1 200 OK\r\n"),
        "{authenticated}"
    );
    let value = response_json(&authenticated);
    assert!(value["available"].as_bool().is_some());
    let object = value.as_object().expect("availability object");
    assert_eq!(
        object.len(),
        2,
        "unexpected availability fields: {object:?}"
    );
    let guidance = value["guidance"].as_str().expect("guidance string");
    assert!(
        guidance == "Claude Code CLI is available to EMP."
            || guidance
                == "Claude Code CLI was not found as an available local installation. Install it or check that EMP can access it, then reopen this form.",
        "unexpected or overly detailed guidance: {guidance}"
    );
    server.shutdown().expect("stop server");
}
