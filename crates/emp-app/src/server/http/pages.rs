use super::{
    error::{EntryError, HttpReply},
    management::page_origin,
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    app::ServerState,
    http::{
        request::{Request, RequestMethod},
        response::HEALTH_JSON_BYTES,
    },
    server::run_blocking,
    services::guide_assets,
    web,
};
use axum::{
    Router,
    body::Bytes,
    extract::{FromRequestParts, State},
    http::request::Parts,
    routing::get,
};
use std::sync::Arc;

enum PageRead {
    Index,
    Health,
    Icon { source: bool },
    Guide(String),
    Asset(HttpReply),
}

impl PageRead {
    fn authorize(request: Request<'_>, port: u16) -> Option<Result<Self, EntryError>> {
        let path = request.raw_path();
        let page = match path {
            "/" | "/index.html" => Self::Index,
            "/healthz" => Self::Health,
            _ if request.method != RequestMethod::Get => return None,
            "/assets/app-icon.png" => Self::Icon { source: false },
            "/assets/app-icon-source.png" => Self::Icon { source: true },
            _ if path.starts_with("/guide/") || path.starts_with("/assets/tutorial-") => {
                Self::Guide(path.to_owned())
            }
            _ => Self::Asset(web::asset_reply(path)?),
        };
        Some(page_origin(request, port).map(|()| page))
    }

    fn read(self, state: &ServerState) -> Result<HttpReply, EntryError> {
        Ok(match self {
            Self::Index => web::ui_reply(),
            Self::Health => HttpReply::new(200, HEALTH_JSON_BYTES),
            Self::Icon { source } => {
                let icon = state.backend.app_icon.snapshot();
                let png = if source { &icon.source_png } else { &icon.png };
                HttpReply::new(200, Bytes::copy_from_slice(png))
                    .with_content_type("image/png")
                    .with_header("Cache-Control", "no-cache")
            }
            Self::Guide(path) => guide_assets::asset_reply(&path).ok_or(EntryError::NotFound)?,
            Self::Asset(reply) => reply,
        })
    }
}

impl FromRequestParts<Arc<ServerState>> for PageRead {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        Self::authorize(raw_head(&parts.extensions)?.request(), state.port)
            .ok_or(EntryError::NotFound)?
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/", get(read).post(read).delete(read))
        .route("/index.html", get(read).post(read).delete(read))
        .route("/healthz", get(read).post(read).delete(read))
        .route("/guide/", get(read))
        .route("/guide/{*path}", get(read))
        .route("/assets/", get(read))
        .route("/assets/{*path}", get(read))
}

async fn read(
    _access: RequestAccess,
    page: PageRead,
    State(state): State<Arc<ServerState>>,
) -> Result<HttpReply, EntryError> {
    #[cfg(test)]
    if matches!(page, PageRead::Health) {
        let probe = state.lifecycle.test_health_handler.lock().unwrap().take();
        if let Some((entered, release)) = probe {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
    run_blocking(move || page.read(&state))
        .await
        .map_err(|_| EntryError::internal())?
}
