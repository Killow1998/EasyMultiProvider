use super::{
    body::RealtimeBody,
    error::{EntryError, HttpReply},
    proxy::{ProxyCaller, RequestAccess, request_source, require_proxy},
    raw_head,
};
use crate::{
    api::{
        realtime::{prepare_call, sideband},
        websocket::{prepare_upgrade, run_responses},
    },
    app::ServerState,
    http::request::{Request, RequestMethod, percent_decode},
    server::{connection::ConnectionContext, run_blocking},
    util::system_now,
};
use axum::{
    Extension, Router,
    body::Body,
    extract::{ConnectInfo, FromRequest, FromRequestParts, Request as AxumRequest, State},
    http::StatusCode,
    response::Response,
    routing::{get, post},
};
use std::{net::SocketAddr, sync::Arc};

const LIVE: &str = "/v1/live";
const SIDEBAND_PREFIX: &str = "/v1/live/";

pub(crate) fn is_responses_upgrade(request: Request<'_>) -> bool {
    request.method == RequestMethod::Get
        && request.raw_path() == "/v1/responses"
        && request
            .header("Upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

pub(in crate::server) fn is_upgrade_route(request: Request<'_>) -> bool {
    is_responses_upgrade(request)
        || (request.method == RequestMethod::Get && request.raw_path().starts_with(SIDEBAND_PREFIX))
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    Router::new()
        .route("/v1/responses", get(responses))
        .route(LIVE, post(create_call))
        .route(SIDEBAND_PREFIX, get(sideband))
        .route("/v1/live/{*call_id}", get(sideband))
}

async fn responses(
    State(state): State<Arc<ServerState>>,
    Extension(context): Extension<ConnectionContext>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: AxumRequest,
) -> Result<Response, HttpReply> {
    let raw = raw_head(request.extensions())
        .map_err(EntryError::reply)?
        .clone();
    if !is_responses_upgrade(raw.request()) {
        return Err(EntryError::NotFound.reply());
    }
    let probe = request
        .extensions_mut()
        .remove::<Arc<tokio::net::TcpStream>>();
    let prepare_state = Arc::clone(&state);
    let (source, accept) = run_blocking({
        let raw = raw.clone();
        move || {
            let mut head = raw.request();
            let now = system_now();
            let source = request_source(head, &prepare_state, now, Some(peer.ip()), None);
            head.source = Some(&source);
            require_proxy(head, &prepare_state, now).map_err(EntryError::reply)?;
            let accept = prepare_upgrade(head, |error| error.to_string())?;
            Ok::<_, HttpReply>((source, accept))
        }
    })
    .await
    .map_err(|_| EntryError::internal().reply())??;
    let connection_id = context.request_id().map(str::to_owned);
    context.on_upgrade(
        hyper::upgrade::on(&mut request),
        None,
        move |io, admitted, _| async move {
            if let Some(io) = io {
                let runtime = state.backend.transport.runtime.handle().clone();
                runtime
                    .spawn(async move {
                        let mut head = raw.request();
                        head.source = Some(&source);
                        head.observation_id = connection_id.as_deref();
                        run_responses(
                            &state,
                            head,
                            connection_id.as_deref(),
                            io,
                            &[],
                            probe,
                            admitted,
                        )
                        .await;
                    })
                    .await
                    .expect("Responses WebSocket session");
            }
        },
    );
    Ok(switching_protocols(&accept))
}

async fn create_call(
    State(state): State<Arc<ServerState>>,
    Extension(context): Extension<ConnectionContext>,
    access: RequestAccess,
    request: AxumRequest,
) -> Result<HttpReply, HttpReply> {
    context.retain_request_access(access);
    let (mut parts, body) = request.into_parts();
    let caller = ProxyCaller::from_request_parts(&mut parts, &state)
        .await
        .map_err(EntryError::reply)?;
    let RealtimeBody(call) =
        RealtimeBody::from_request(AxumRequest::from_parts(parts, body), &state)
            .await
            .map_err(EntryError::reply)?;
    let prepare_state = Arc::clone(&state);
    let call = run_blocking(move || prepare_call(&prepare_state, caller.incoming, call))
        .await
        .map_err(|_| EntryError::internal().reply())??;
    Ok(call.send(&state).await)
}

async fn sideband(
    State(state): State<Arc<ServerState>>,
    Extension(context): Extension<ConnectionContext>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut request: AxumRequest,
) -> Result<Response, HttpReply> {
    let raw = raw_head(request.extensions())
        .map_err(EntryError::reply)?
        .clone();
    let prepare_state = Arc::clone(&state);
    let prepared = run_blocking(move || {
        let mut head = raw.request();
        let now = system_now();
        let source = request_source(head, &prepare_state, now, Some(peer.ip()), None);
        head.source = Some(&source);
        let call_id = percent_decode(&head.raw_path()[SIDEBAND_PREFIX.len()..], false);
        sideband::prepare_sideband(head, &call_id, &prepare_state, now)
    })
    .await
    .map_err(|_| EntryError::internal().reply())??;
    let mut upstream = prepared.connect().await?;
    let response = switching_protocols(&prepared.accept);
    let maximum = prepared.max_message_bytes;
    context.on_upgrade(
        hyper::upgrade::on(&mut request),
        Some(prepared.permit),
        move |io, _, _| async move {
            if let Some(io) = io {
                sideband::relay::run(&state, upstream, io, &[], maximum).await;
            } else {
                upstream.close(1001, "downstream disconnected").await;
            }
        },
    );
    Ok(response)
}

fn switching_protocols(accept: &str) -> Response {
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept)
        .body(Body::empty())
        .expect("validated WebSocket handshake")
}
