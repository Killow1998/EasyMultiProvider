use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    proxy::RequestAccess,
    raw_head,
};
use crate::app::ServerState;
use crate::http::{
    auth::{BOOTSTRAP_HEADER, CLEAR_LEGACY_SESSION_COOKIE, SESSION_HEADER, constant_time_eq},
    request::{Request, RequestMethod},
};
use crate::server::{connection::ConnectionContext, run_blocking};
use crate::util::system_now;
use axum::{
    Extension,
    extract::{FromRequest, FromRequestParts, State},
    http::request::Parts,
    routing::{MethodFilter, MethodRouter, get, on, post},
};
use std::sync::Arc;

pub(crate) fn session_header_value(request: Request<'_>) -> Option<String> {
    if request.header_count(SESSION_HEADER) != 1 {
        return None;
    }
    let value = request.header(SESSION_HEADER)?;
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic()))
        .then(|| value.to_owned())
}

pub(crate) fn same_origin(request: Request<'_>, port: u16) -> bool {
    let allowed_hosts = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
    if request.header_count("Host") != 1 || request.header_count("Origin") > 1 {
        return false;
    }
    let Some(host) = request.header("Host") else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if !allowed_hosts.iter().any(|allowed| allowed == host) {
        return false;
    }

    let Some(origin) = request.header("Origin") else {
        return true;
    };
    match parse_origin(origin, port) {
        Some(true) => true,
        Some(false) | None => false,
    }
}

fn parse_origin(origin: &str, port: u16) -> Option<bool> {
    let (scheme, rest) = origin.split_once("://")?;
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return Some(false);
    }
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Some(false);
    }
    let (host, origin_port) = if let Some(host) = authority.strip_prefix('[') {
        let Some((host, suffix)) = host.split_once(']') else {
            return Some(false);
        };
        let port = suffix.strip_prefix(':')?;
        (host, port)
    } else {
        let (host, origin_port) = authority.rsplit_once(':')?;
        (host, origin_port)
    };
    let host = host.to_ascii_lowercase();
    let origin_port = origin_port.parse::<u16>().ok()?;
    Some((host == "127.0.0.1" || host == "localhost") && origin_port == port)
}

pub(crate) const SESSION_PATH: &str = "/api/session";

pub(in crate::server) fn session_route() -> MethodRouter<Arc<ServerState>> {
    post(exchange_session)
}

async fn exchange_session(
    access: RequestAccess,
    exchange: BootstrapExchange,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
) -> Result<JsonReply, EntryError> {
    let now = system_now();
    connection.retain_request_access(access);
    run_blocking(move || {
        let _connection = connection;
        exchange.execute(&state, now)
    })
    .await
    .map_err(|_| EntryError::internal())?
}

pub(crate) struct ManagementSession(pub(crate) String);

pub(crate) type ManagementRead = fn(&ServerState) -> JsonReply;

pub(super) fn read_route(read: ManagementRead) -> MethodRouter<Arc<ServerState>> {
    get(
        move |_access: RequestAccess,
              _session: ManagementSession,
              State(state): State<Arc<ServerState>>| async move {
            run_blocking(move || read(&state))
                .await
                .map_err(|_| EntryError::internal())
        },
    )
}

pub(super) fn write_route<T: Send + 'static>(
    methods: MethodFilter,
    execute: fn(T, &ServerState, Option<&str>) -> JsonReply,
) -> MethodRouter<Arc<ServerState>>
where
    JsonBody<T>: FromRequest<Arc<ServerState>, Rejection = EntryError>,
{
    on(
        methods,
        move |access: RequestAccess,
              _session: ManagementSession,
              State(state): State<Arc<ServerState>>,
              Extension(connection): Extension<ConnectionContext>,
              JsonBody(command): JsonBody<T>| async move {
            connection.retain_request_access(access);
            run_blocking(move || execute(command, &state, connection.request_id()))
                .await
                .map_err(|_| EntryError::internal())
        },
    )
}

impl ManagementSession {
    pub(crate) fn require(
        request: Request<'_>,
        state: &ServerState,
        now: f64,
    ) -> Result<Self, EntryError> {
        if !same_origin(request, state.port) {
            return Err(match (request.method, request.raw_path()) {
                (RequestMethod::Post, "/api/modules")
                | (RequestMethod::Post | RequestMethod::Delete, "/api/gateway/clients") => {
                    EntryError::ManagementSession
                }
                (RequestMethod::Post, path) if path.starts_with("/api/team/") => {
                    EntryError::ManagementSession
                }
                _ => EntryError::CrossOrigin("management session is required"),
            });
        }
        request
            .session_token()
            .filter(|token| state.sessions.contains(Some(token), now))
            .map(Self)
            .ok_or(EntryError::ManagementSession)
    }
}

impl FromRequestParts<Arc<ServerState>> for ManagementSession {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        Self::require(raw_head(&parts.extensions)?.request(), state, system_now())
    }
}

pub(crate) struct BootstrapExchange(String);

impl BootstrapExchange {
    pub(crate) fn parse(request: Request<'_>, port: u16) -> Result<Self, EntryError> {
        if !same_origin(request, port) {
            return Err(EntryError::CrossOrigin("management session is required"));
        }
        (request.header_count(BOOTSTRAP_HEADER) == 1)
            .then(|| request.header(BOOTSTRAP_HEADER))
            .flatten()
            .filter(|value| !value.is_empty() && value.is_ascii())
            .map(|value| Self(value.to_owned()))
            .ok_or(EntryError::ManagementSession)
    }

    pub(crate) fn execute(self, state: &ServerState, now: f64) -> Result<JsonReply, EntryError> {
        let reopened = state.reopen_bootstrap.lock().ok();
        let bootstrap = if state.bootstrap.matches(&self.0, now) {
            &state.bootstrap
        } else if let Some(token) = reopened
            .as_ref()
            .and_then(|guard| guard.as_ref())
            .filter(|token| token.matches(&self.0, now))
        {
            token
        } else {
            return Err(EntryError::ManagementSession);
        };
        if !bootstrap.consume() {
            return Err(EntryError::ManagementSession);
        }
        let Some((token, expires_in)) = state.sessions.rotate(now) else {
            bootstrap.release();
            return Err(EntryError::internal());
        };
        let body = serde_json::json!({"session":token,"expires_in":expires_in})
            .to_string()
            .into_bytes();
        Ok(JsonReply::new(200, body).with_header("Set-Cookie", CLEAR_LEGACY_SESSION_COOKIE))
    }
}

impl FromRequestParts<Arc<ServerState>> for BootstrapExchange {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        Self::parse(raw_head(&parts.extensions)?.request(), state.port)
    }
}

pub(crate) fn page_origin(request: Request<'_>, port: u16) -> Result<(), EntryError> {
    if same_origin(request, port) {
        return Ok(());
    }
    let path = request.raw_path();
    Err(EntryError::CrossOrigin(if path == "/healthz" {
        "cross-origin health request rejected"
    } else if path.starts_with("/guide/") || path.starts_with("/assets/tutorial-") {
        "cross-origin guide request rejected"
    } else {
        "cross-origin Web UI request rejected"
    }))
}

#[cfg(test)]
pub(crate) struct PageOrigin;

#[cfg(test)]
impl FromRequestParts<Arc<ServerState>> for PageOrigin {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        page_origin(raw_head(&parts.extensions)?.request(), state.port)?;
        Ok(Self)
    }
}

#[derive(Clone)]
pub(crate) struct RecoveryAccess {
    pub(crate) port: u16,
    pub(crate) token: Arc<str>,
}

pub(crate) fn recovery_origin(request: Request<'_>, port: u16) -> Result<(), EntryError> {
    if !same_origin(request, port) {
        return Err(EntryError::Recovery(403, "origin_rejected"));
    }
    Ok(())
}

pub(crate) fn recovery_session(request: Request<'_>, token: &str) -> Result<(), EntryError> {
    if request.header_count("X-EMP-Recovery") != 1
        || !request
            .header("X-EMP-Recovery")
            .is_some_and(|supplied| constant_time_eq(supplied.as_bytes(), token.as_bytes()))
    {
        return Err(EntryError::Recovery(401, "recovery_session_required"));
    }
    Ok(())
}

pub(crate) enum RecoveryMutation {
    Exit,
    Reset { keep_backup: bool },
}

pub(crate) fn recovery_mutation(
    request: Request<'_>,
    body_prefix_len: usize,
) -> Result<RecoveryMutation, EntryError> {
    if request.method != RequestMethod::Post
        || request.header_count("Content-Length") != 1
        || request.header("Content-Length") != Some("0")
        || request.header_count("Transfer-Encoding") != 0
        || body_prefix_len != 0
    {
        return Err(EntryError::Recovery(400, "invalid_request"));
    }
    if request.target == "/api/recovery/exit" {
        return Ok(RecoveryMutation::Exit);
    }
    if request.target == "/api/recovery/reset"
        && request.header_count("X-EMP-Recovery-Confirm") == 1
    {
        match request.header("X-EMP-Recovery-Confirm") {
            Some("clear-and-reset") => return Ok(RecoveryMutation::Reset { keep_backup: false }),
            Some("backup-and-reset") => return Ok(RecoveryMutation::Reset { keep_backup: true }),
            _ => {}
        }
    }
    Err(EntryError::Recovery(409, "confirmation_required"))
}

pub(crate) struct RecoveryOrigin;

impl FromRequestParts<RecoveryAccess> for RecoveryOrigin {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RecoveryAccess,
    ) -> Result<Self, EntryError> {
        recovery_origin(raw_head(&parts.extensions)?.request(), state.port)?;
        Ok(Self)
    }
}

pub(crate) struct RecoverySession;

impl FromRequestParts<RecoveryAccess> for RecoverySession {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RecoveryAccess,
    ) -> Result<Self, EntryError> {
        let request = raw_head(&parts.extensions)?.request();
        recovery_origin(request, state.port)?;
        recovery_session(request, &state.token)?;
        Ok(Self)
    }
}

impl FromRequestParts<RecoveryAccess> for RecoveryMutation {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &RecoveryAccess,
    ) -> Result<Self, EntryError> {
        RecoverySession::from_request_parts(parts, state).await?;
        let raw = raw_head(&parts.extensions)?;
        recovery_mutation(raw.request(), raw.body_prefix_len())
    }
}

pub(crate) fn issue_export_confirmation(
    state: &ServerState,
    now: f64,
) -> Result<JsonReply, EntryError> {
    use crate::http::auth::{EXPORT_CONFIRMATION_LIFETIME_SECONDS, EXPORT_CONFIRMATION_OPERATION};
    let confirmation = state
        .sessions
        .issue_export_confirmation(EXPORT_CONFIRMATION_OPERATION, now)
        .ok_or_else(EntryError::internal)?;
    Ok(JsonReply::new(
        200,
        serde_json::json!({
            "confirmation": confirmation,
            "expires_in": EXPORT_CONFIRMATION_LIFETIME_SECONDS as u64,
        })
        .to_string()
        .into_bytes(),
    ))
}

pub(crate) fn consume_export_confirmation(
    state: &ServerState,
    supplied: &str,
    path: &str,
    now: f64,
) -> Result<(), EntryError> {
    if state.sessions.consume_export_confirmation(
        supplied,
        crate::http::auth::EXPORT_CONFIRMATION_OPERATION,
        now,
    ) {
        return Ok(());
    }
    let message = if path == "/api/migration/lan/start" {
        "export confirmation is required"
    } else {
        "export confirmation is missing or expired; confirm the export again"
    };
    Err(EntryError::Json(
        403,
        message,
        Some("export_confirmation_required"),
    ))
}
