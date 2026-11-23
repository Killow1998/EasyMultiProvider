use super::*;
use crate::http::request::{RequestMethod, parse_request, read_request_head_before};
use crate::http::response::response;
use crate::server::http::management::{
    RecoveryMutation, recovery_mutation, recovery_origin, recovery_session,
};
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
    let english =
        emp_state::load_configuration(Some(config)).is_ok_and(|config| config["language"] == "en");
    if let Some(mut tray) = open_browser
        .then(|| crate::tray::Recovery::new(url, english))
        .flatten()
    {
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_stopped = stopped.clone();
        let config = config.to_path_buf();
        let worker = std::thread::Builder::new()
            .name("emp-recovery".to_owned())
            .spawn(move || serve(listener, &config, reason, &token, &worker_stopped))?;
        while !worker.is_finished() {
            if !tray.poll() {
                stopped.store(true, std::sync::atomic::Ordering::Release);
                let _ = TcpStream::connect(("127.0.0.1", port));
                break;
            }
        }
        let result = worker.join().map_err(|_| AppError::ServerStopped)?;
        return if stopped.load(std::sync::atomic::Ordering::Acquire) {
            Ok(false)
        } else {
            result
        };
    }
    serve(
        listener,
        config,
        reason,
        &token,
        &std::sync::atomic::AtomicBool::new(false),
    )
}

pub(super) fn serve(
    listener: TcpListener,
    config: &Path,
    reason: &str,
    token: &str,
    stopped: &std::sync::atomic::AtomicBool,
) -> Result<bool, AppError> {
    let port = listener.local_addr()?.port();
    for stream in listener.incoming() {
        let mut stream = stream?;
        if stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(false);
        }
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let Some(raw) =
            read_request_head_before(&mut stream, Instant::now() + Duration::from_secs(3))
        else {
            continue;
        };
        let Some(request) = parse_request(&raw.head) else {
            continue;
        };
        if let Err(error) = recovery_origin(request, port) {
            let _ = stream.write_all(&error.wire_response());
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
        if request.method == RequestMethod::Get
            && matches!(
                request.target,
                "/assets/i18n.js" | "/assets/copy-recovery.js"
            )
        {
            if let Some(asset) = crate::web::asset_response(request.target) {
                let _ = stream.write_all(&asset);
            }
            continue;
        }
        if let Err(error) = recovery_session(request, token) {
            let _ = stream.write_all(&error.wire_response());
            continue;
        }
        if request.method == RequestMethod::Get && request.target == "/api/recovery" {
            send(&mut stream, 200, serde_json::json!({"reason":reason}));
            continue;
        }
        match recovery_mutation(request, raw.body_prefix.len()) {
            Err(error) => {
                let _ = stream.write_all(&error.wire_response());
            }
            Ok(RecoveryMutation::Exit) => {
                send(&mut stream, 200, serde_json::json!({"exiting":true}));
                return Ok(false);
            }
            Ok(RecoveryMutation::Reset { keep_backup }) => {
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
