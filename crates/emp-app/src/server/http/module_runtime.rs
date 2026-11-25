use super::{
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::{RequestAccess, local_caller, request_source, require_local_proxy, require_proxy},
};
use crate::{
    app::ServerState,
    http::request::module_runtime_target,
    server::{connection::ConnectionContext, head::RawRequestHead, run_blocking},
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::{ConnectInfo, State},
    routing::any,
};
use emp_modules::manifest::RouteAuth;
use std::{net::SocketAddr, sync::Arc};

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/api/module-runtime/{module_id}", any(dispatch))
        .route("/api/module-runtime/{module_id}/", any(dispatch))
        .route("/api/module-runtime/{module_id}/{*path}", any(dispatch))
}

async fn dispatch(
    State(state): State<Arc<ServerState>>,
    Extension(raw): Extension<RawRequestHead>,
    Extension(connection): Extension<ConnectionContext>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<JsonReply, EntryError> {
    let request = raw.request();
    let target = module_runtime_target(request.raw_path()).ok_or(EntryError::NotFound)?;
    let (module_id, path) = target.split_at(target.find('/').ok_or(EntryError::NotFound)?);
    let module = state
        .backend
        .modules
        .registry
        .get(module_id)
        .ok_or(EntryError::NotFound)?;
    let methods = module.routes.get(path).ok_or(EntryError::NotFound)?;
    let Some(route) = methods.get(request.method.as_str()) else {
        let allow = methods
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Ok(JsonReply::new(405, "")
            .with_status_text("Method Not Allowed")
            .with_header("Allow", allow));
    };

    let caller = match route.auth {
        RouteAuth::Management => {
            ManagementSession::require(request, &state, system_now())?;
            local_caller()
        }
        RouteAuth::Proxy => {
            let state = Arc::clone(&state);
            let raw = raw.clone();
            run_blocking(move || {
                let mut request = raw.request();
                let now = system_now();
                let source = request_source(request, &state, now, Some(peer.ip()), None);
                request.source = Some(&source);
                require_proxy(request, &state, now)?;
                Ok::<_, EntryError>(source.caller.expect("authorized proxy caller"))
            })
            .await
            .map_err(|_| EntryError::internal())??
        }
        RouteAuth::LocalProxy => require_local_proxy(request, &state, peer.ip())?,
    };
    connection.retain_request_access(RequestAccess::require(request, &state, None)?);
    state
        .lifecycle
        .test_hooks
        .module_route(&route.id, &module, &caller);
    Err(EntryError::Json(
        503,
        "module data endpoint is unavailable",
        None,
    ))
}
