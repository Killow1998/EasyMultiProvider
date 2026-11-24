use super::*;
use crate::http::request::RequestMethod;
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::http::request::{parse_request, read_request_head_before};
#[cfg(any(test, not(feature = "server-candidate")))]
use crate::server::http::management::recovery_origin;
use crate::server::http::management::{RecoveryMutation, recovery_mutation, recovery_session};
use crate::server::{ServerControl, ShutdownReason, http::error::HttpReply};
use crate::services::connection_admission::{ConnectionAdmission, ConnectionAdmissionConfig};
#[cfg(not(feature = "server-candidate"))]
use std::net::TcpStream;
use std::time::Duration;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::time::Instant;
#[cfg(any(test, not(feature = "server-candidate")))]
use std::{
    io::Write,
    sync::atomic::{AtomicBool, Ordering},
};
use std::{net::TcpListener, sync::Arc};

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
    let control = Arc::new(ServerControl::recovery(Arc::new(ConnectionAdmission::new(
        ConnectionAdmissionConfig::default(),
    ))));
    if let Some(mut tray) = open_browser
        .then(|| crate::tray::Recovery::new(url, english))
        .flatten()
    {
        let worker_control = control.clone();
        let mut tray_closed = false;
        let config = config.to_path_buf();
        let worker = std::thread::Builder::new()
            .name("emp-recovery".to_owned())
            .spawn(move || serve_selected(listener, &config, reason, &token, worker_control))?;
        while !worker.is_finished() {
            if !tray.poll() {
                tray_closed = true;
                control.stop(ShutdownReason::Requested);
                #[cfg(not(feature = "server-candidate"))]
                let _ = TcpStream::connect(("127.0.0.1", port));
                break;
            }
        }
        let result = worker.join().map_err(|_| AppError::ServerStopped)?;
        return if tray_closed { Ok(false) } else { result };
    }
    serve_selected(listener, config, reason, &token, control)
}

fn serve_selected(
    listener: TcpListener,
    config: &Path,
    reason: &'static str,
    token: &str,
    control: Arc<ServerControl>,
) -> Result<bool, AppError> {
    #[cfg(not(feature = "server-candidate"))]
    return serve(listener, config, reason, token, &control.shutdown);
    #[cfg(feature = "server-candidate")]
    return serve_candidate(listener, config, reason, token, control);
}

#[cfg(any(test, not(feature = "server-candidate")))]
pub(super) fn serve(
    listener: TcpListener,
    config: &Path,
    reason: &str,
    token: &str,
    stopped: &AtomicBool,
) -> Result<bool, AppError> {
    let port = listener.local_addr()?.port();
    for stream in listener.incoming() {
        let mut stream = stream?;
        if stopped.load(Ordering::Acquire) {
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
        if let Some(reply) = public_reply(request) {
            let _ = stream.write_all(&reply.wire_response());
            continue;
        }
        if let Err(error) = recovery_session(request, token) {
            let _ = stream.write_all(&error.wire_response());
            continue;
        }
        if request.method == RequestMethod::Get && request.target == "/api/recovery" {
            let _ = stream
                .write_all(&json_reply(200, serde_json::json!({"reason":reason})).wire_response());
            continue;
        }
        match recovery_mutation(request, raw.body_prefix.len()) {
            Err(error) => {
                let _ = stream.write_all(&error.wire_response());
            }
            Ok(RecoveryMutation::Exit) => {
                let _ = stream.write_all(&exit_reply().wire_response());
                return Ok(false);
            }
            Ok(RecoveryMutation::Reset { keep_backup }) => {
                let (reply, restart) = reset_reply(config, keep_backup);
                let _ = stream.write_all(&reply.wire_response());
                if restart {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn public_reply(request: crate::http::request::Request<'_>) -> Option<HttpReply> {
    if request.method != RequestMethod::Get {
        return None;
    }
    match request.target {
        "/" => Some(
            HttpReply::new(200, PAGE)
                .with_content_type("text/html; charset=utf-8")
                .with_header("Cache-Control", "no-store")
                .with_header("Referrer-Policy", "no-referrer"),
        ),
        "/healthz" => Some(json_reply(
            503,
            serde_json::json!({"status":"recovery_required"}),
        )),
        path @ ("/assets/i18n.js" | "/assets/copy-recovery.js") => crate::web::asset_reply(path),
        _ => None,
    }
}

fn json_reply(status: u16, body: Value) -> HttpReply {
    HttpReply::new(status, body.to_string())
}

fn exit_reply() -> HttpReply {
    json_reply(200, serde_json::json!({"exiting":true}))
}

fn reset_reply(config: &Path, keep_backup: bool) -> (HttpReply, bool) {
    match reset::rebuild(config, keep_backup) {
        Ok((backup, cleanup_pending)) => (
            json_reply(
                200,
                serde_json::json!({"restarting":true,"backup_path":backup,"cleanup_pending":cleanup_pending}),
            ),
            true,
        ),
        Err(error) => (json_reply(409, serde_json::json!({"error":error})), false),
    }
}

#[cfg(any(test, feature = "server-candidate"))]
pub(super) fn serve_candidate(
    listener: TcpListener,
    config: &Path,
    reason: &'static str,
    token: &str,
    control: Arc<ServerControl>,
) -> Result<bool, AppError> {
    use crate::server::http::management::RecoveryAccess;
    use axum::routing::{get, post};

    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let router = axum::Router::new()
            .route("/", get(public))
            .route("/healthz", get(public))
            .route("/assets/i18n.js", get(public))
            .route("/assets/copy-recovery.js", get(public))
            .route("/api/recovery", get(status))
            .route("/api/recovery/reset", post(mutate))
            .route("/api/recovery/exit", post(mutate))
            .fallback(unmatched)
            .method_not_allowed_fallback(unmatched)
            .layer(axum::Extension(RecoveryDetails {
                config: config.to_path_buf(),
                reason,
            }))
            .with_state(RecoveryAccess {
                port,
                token: Arc::from(token),
            });
        let stopped = control.signal();
        crate::server::serve_recovery(listener, router, control)
            .await
            .map_err(|_| AppError::ServerStopped)?;
        Ok(stopped.wait().await == ShutdownReason::RecoveryReset)
    })
}

#[cfg(any(test, feature = "server-candidate"))]
use crate::server::{
    connection::ConnectionContext,
    head::RawRequestHead,
    http::{
        error::EntryError,
        management::{RecoveryAccess, RecoveryOrigin, RecoverySession},
    },
    run_blocking,
};
#[cfg(any(test, feature = "server-candidate"))]
use axum::{Extension, extract::State};

#[cfg(any(test, feature = "server-candidate"))]
#[derive(Clone)]
struct RecoveryDetails {
    config: PathBuf,
    reason: &'static str,
}

#[cfg(any(test, feature = "server-candidate"))]
async fn public(
    _origin: RecoveryOrigin,
    State(access): State<RecoveryAccess>,
    Extension(raw): Extension<RawRequestHead>,
) -> Result<HttpReply, EntryError> {
    if let Some(reply) = public_reply(raw.request()) {
        return Ok(reply);
    }
    recovery_session(raw.request(), &access.token)?;
    Err(EntryError::Recovery(400, "invalid_request"))
}

#[cfg(any(test, feature = "server-candidate"))]
async fn status(
    _session: RecoverySession,
    Extension(details): Extension<RecoveryDetails>,
    Extension(raw): Extension<RawRequestHead>,
) -> Result<HttpReply, EntryError> {
    if raw.request().method == RequestMethod::Get && raw.request().target == "/api/recovery" {
        Ok(json_reply(
            200,
            serde_json::json!({"reason":details.reason}),
        ))
    } else {
        Err(EntryError::Recovery(400, "invalid_request"))
    }
}

#[cfg(any(test, feature = "server-candidate"))]
async fn mutate(
    mutation: RecoveryMutation,
    Extension(context): Extension<ConnectionContext>,
    Extension(details): Extension<RecoveryDetails>,
) -> Result<HttpReply, EntryError> {
    let complete = context.shutdown_after_response();
    match mutation {
        RecoveryMutation::Exit => {
            let _ = complete.send(ShutdownReason::Requested);
            Ok(exit_reply())
        }
        RecoveryMutation::Reset { keep_backup } => run_blocking(move || {
            let (reply, restart) = reset_reply(&details.config, keep_backup);
            if restart {
                let _ = complete.send(ShutdownReason::RecoveryReset);
            }
            reply
        })
        .await
        .map_err(|_| EntryError::internal()),
    }
}

#[cfg(any(test, feature = "server-candidate"))]
async fn unmatched(
    _session: RecoverySession,
    Extension(raw): Extension<RawRequestHead>,
) -> EntryError {
    recovery_mutation(raw.request(), raw.body_prefix_len())
        .err()
        .unwrap_or(EntryError::Recovery(400, "invalid_request"))
}
