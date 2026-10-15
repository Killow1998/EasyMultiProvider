use super::*;
use crate::http::auth::{constant_time_eq, same_origin};
use crate::http::request::{RequestMethod, parse_request, read_request_head_before};
use crate::http::response::response;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

const PAGE: &[u8] = include_bytes!("../../web/recovery.html");

/// Returns true to retry normal startup, false for the explicit Exit action.
/// No normal backend, credential workers or Codex integration runs here.
pub(crate) fn run(config: &Path, error: &AppError, open_browser: bool) -> Result<bool, AppError> {
    let root = config.parent().unwrap_or(Path::new("."));
    let _owner = emp_state::IntegrationFileLock::acquire(
        &root.join("state/service.lock"),
        Duration::ZERO,
        Duration::from_millis(20),
    )
    .map_err(|_| AppError::ServiceOwned)?;
    // A recovery listener never displaces an existing service on the target port.
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = crate::util::random_hex(32)?;
    let url = format!("http://127.0.0.1:{port}/#recovery={token}");
    println!("EMP startup needs attention ({})", issue(error));
    println!("Recovery page: {url}");
    if open_browser {
        crate::cli::desktop::open_browser(&url);
    }
    let reason = issue(error);
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let Some(raw) =
            read_request_head_before(&mut stream, Instant::now() + Duration::from_secs(3))
        else {
            continue;
        };
        let Some(request) = parse_request(&raw.head) else {
            continue;
        };
        if !same_origin(request, port) {
            send(
                &mut stream,
                403,
                serde_json::json!({"error":"origin_rejected"}),
            );
            continue;
        }
        if request.method == RequestMethod::Get && request.target == "/" {
            let _ = stream.write_all(&response(
                "HTTP/1.1 200 OK",
                "text/html; charset=utf-8",
                PAGE,
                &[
                    ("Cache-Control", "no-store"),
                    ("Referrer-Policy", "no-referrer"),
                ],
            ));
            continue;
        }
        if request.method == RequestMethod::Get && request.target == "/healthz" {
            send(
                &mut stream,
                503,
                serde_json::json!({"status":"recovery_required"}),
            );
            continue;
        }
        if request.header_count("X-EMP-Recovery") != 1
            || !request
                .header("X-EMP-Recovery")
                .is_some_and(|supplied| constant_time_eq(supplied.as_bytes(), token.as_bytes()))
        {
            send(
                &mut stream,
                401,
                serde_json::json!({"error":"recovery_session_required"}),
            );
            continue;
        }
        if request.method == RequestMethod::Get && request.target == "/api/recovery" {
            send(&mut stream, 200, serde_json::json!({"reason":reason}));
            continue;
        }
        // Actions have no body. Reject ambiguous framing instead of leaving
        // unread bytes that could be interpreted as another command.
        if request.method != RequestMethod::Post
            || request.header_count("Content-Length") != 1
            || request.header("Content-Length") != Some("0")
            || request.header_count("Transfer-Encoding") != 0
            || !raw.body_prefix.is_empty()
        {
            send(
                &mut stream,
                400,
                serde_json::json!({"error":"invalid_request"}),
            );
            continue;
        }
        match request.target {
            "/api/recovery/exit" => {
                send(&mut stream, 200, serde_json::json!({"exiting":true}));
                return Ok(false);
            }
            "/api/recovery/reset"
                if request.header_count("X-EMP-Recovery-Confirm") == 1
                    && matches!(
                        request.header("X-EMP-Recovery-Confirm"),
                        Some("clear-and-reset" | "backup-and-reset")
                    ) =>
            {
                let keep_backup =
                    request.header("X-EMP-Recovery-Confirm") == Some("backup-and-reset");
                match reset::rebuild(config, keep_backup) {
                    Ok((backup, cleanup_pending)) => {
                        send(
                            &mut stream,
                            200,
                            serde_json::json!({"restarting":true,"backup_path":backup,"cleanup_pending":cleanup_pending}),
                        );
                        return Ok(true);
                    }
                    Err(error) => {
                        send(&mut stream, 409, serde_json::json!({"error":error}));
                    }
                }
            }
            _ => send(
                &mut stream,
                409,
                serde_json::json!({"error":"confirmation_required"}),
            ),
        }
    }
    Ok(false)
}

fn send(stream: &mut TcpStream, status: u16, body: Value) {
    let status = format!(
        "HTTP/1.1 {status} {}",
        crate::http::response::status_text(status)
    );
    let _ = stream.write_all(&response(
        &status,
        "application/json",
        body.to_string().as_bytes(),
        &[],
    ));
}
