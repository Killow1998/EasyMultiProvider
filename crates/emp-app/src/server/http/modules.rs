use super::{
    error::{EntryError, HttpReply, JsonReply},
    management::{ManagementSession, same_origin},
    proxy::RequestAccess,
    raw_head,
};
use crate::{api::modules, app::ServerState, http::request::Request, server::run_blocking};
use axum::{
    Router,
    extract::{FromRequestParts, State},
    http::request::Parts,
    routing::get,
};
use emp_modules::Module;
use std::sync::Arc;

pub(crate) struct ModulePanel(Module);

impl ModulePanel {
    pub(crate) fn for_path(path: &str) -> Option<Self> {
        let id = path.strip_prefix("/assets/module-")?.strip_suffix(".js")?;
        Module::ALL
            .into_iter()
            .find(|module| module.id() == id)
            .map(Self)
    }

    pub(crate) fn require_origin(
        self,
        request: Request<'_>,
        port: u16,
    ) -> Result<Self, EntryError> {
        if same_origin(request, port) {
            Ok(self)
        } else {
            Err(EntryError::NotFound)
        }
    }

    pub(crate) fn read(self, request: Request<'_>, state: &ServerState) -> HttpReply {
        let revision = request.target.split_once('?').and_then(|(_, query)| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix("revision="))
        });
        match state.backend.modules.panel(self.0, revision) {
            Some(source) => HttpReply::new(200, source.into_bytes())
                .with_content_type("text/javascript; charset=utf-8")
                .with_header("Cache-Control", "no-store"),
            None => HttpReply::new(404, Vec::new()).with_content_type("text/plain"),
        }
    }
}

impl FromRequestParts<Arc<ServerState>> for ModulePanel {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let request = raw_head(&parts.extensions)?.request();
        Self::for_path(request.raw_path())
            .ok_or(EntryError::NotFound)?
            .require_origin(request, state.port)
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/modules", get(read))
        .route("/assets/{asset}", get(read_panel))
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
) -> Result<JsonReply, EntryError> {
    run_blocking(move || modules::read(&state))
        .await
        .map_err(|_| EntryError::internal())
}

async fn read_panel(
    _access: RequestAccess,
    panel: ModulePanel,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<HttpReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || panel.read(raw.request(), &state))
        .await
        .map_err(|_| EntryError::internal())
}
