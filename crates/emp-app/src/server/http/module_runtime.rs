use super::{
    body::{BodyError, MANAGEMENT_BODY_LIMIT, body_size_error, read_frame, request_length},
    error::{EntryError, JsonReply, insert_security_headers},
    management::ManagementSession,
    proxy::{RequestAccess, local_caller, request_source, require_local_proxy, require_proxy},
};
use crate::{
    app::ServerState,
    http::{
        auth::{BOOTSTRAP_HEADER, SESSION_HEADER},
        request::{module_runtime_target, request_body_timeout, reserved_source_header},
    },
    modules::process::DataEndpoint,
    server::{connection::ConnectionContext, head::RawRequestHead, run_blocking},
    util::system_now,
};
use axum::{
    Extension, Router,
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, Uri, Version, header},
    response::{IntoResponse, Response},
    routing::any,
};
use emp_modules::{handshake::ROUTE_ID_HEADER, manifest::RouteAuth};
use futures_util::{StreamExt as _, stream};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::{
    net::TcpStream,
    sync::oneshot,
    task::JoinSet,
    time::{Instant, timeout_at},
};

const UNAVAILABLE: EntryError = EntryError::Json(503, "module data endpoint is unavailable", None);
const BAD_GATEWAY: EntryError = EntryError::Json(502, "module data request failed", None);

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
    incoming: Request,
) -> Result<Response, EntryError> {
    let request = raw.request();
    let target = module_runtime_target(request.target).ok_or(EntryError::NotFound)?;
    let (path, _) = target.split_once('?').unwrap_or((target, ""));
    let (module_id, path) = path.split_at(path.find('/').ok_or(EntryError::NotFound)?);
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
            .with_header("Allow", allow)
            .into_response());
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
    let endpoint = module.data_endpoint.clone().ok_or(UNAVAILABLE)?;
    let target = super::routing_path(&target[module_id.len()..]);
    let target = target.parse::<Uri>().map_err(|_| EntryError::InvalidHead)?;
    let length = request_length(request, MANAGEMENT_BODY_LIMIT)?;
    if length > MANAGEMENT_BODY_LIMIT {
        return Err(body_size_error(MANAGEMENT_BODY_LIMIT, false).into());
    }
    forward(incoming, target, raw, endpoint, &route.id, length).await
}

async fn forward(
    request: Request,
    target: Uri,
    raw: RawRequestHead,
    mut endpoint: DataEndpoint,
    route_id: &str,
    length: usize,
) -> Result<Response, EntryError> {
    let route_id = HeaderValue::from_str(route_id).map_err(|_| BAD_GATEWAY)?;
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port.get()));
    let socket = tokio::select! {
        biased;
        _ = endpoint.disconnected.wait_for(|closed| *closed) => return Err(UNAVAILABLE),
        socket = TcpStream::connect(address) => socket.map_err(|_| BAD_GATEWAY)?,
    };
    let (mut sender, connection) = http1::handshake(TokioIo::new(socket))
        .await
        .map_err(|_| BAD_GATEWAY)?;

    let (mut parts, body) = request.into_parts();
    parts.uri = target;
    parts.version = Version::HTTP_11;
    parts.extensions.clear();
    strip_headers(&mut parts.headers, |name| {
        reserved_source_header(name)
            || name.starts_with("x-forwarded-")
            || name.starts_with("sec-fetch-")
            || [SESSION_HEADER, BOOTSTRAP_HEADER]
                .iter()
                .any(|header| name.eq_ignore_ascii_case(header))
            || matches!(
                name,
                "authorization"
                    | "cookie"
                    | "cookie2"
                    | "forwarded"
                    | "host"
                    | "origin"
                    | "referer"
                    | "x-real-ip"
                    | "x-aero-caller-key"
                    | "x-emp-recovery"
                    | "x-emp-recovery-confirm"
            )
    });
    parts.headers.insert(
        header::HOST,
        HeaderValue::from_str(&address.to_string()).expect("loopback host"),
    );
    parts
        .headers
        .insert(header::AUTHORIZATION, endpoint.authorization);
    parts.headers.insert(ROUTE_ID_HEADER, route_id);
    parts
        .headers
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    if length > 0 {
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }

    raw.allow_body();
    let deadline = Instant::now() + request_body_timeout(length);
    let failure = Arc::new(Mutex::new(None));
    let (upload_finished, uploaded) = oneshot::channel::<()>();
    let upload = if length == 0 {
        drop(upload_finished);
        Body::empty()
    } else {
        Body::from_stream(stream::try_unfold(
            Some((body, length, upload_finished, failure.clone())),
            move |state| async move {
                let Some((mut body, remaining, finished, failure)) = state else {
                    return Ok(None);
                };
                let chunk = read_frame(&mut body, deadline).await.and_then(|frame| {
                    frame
                        .and_then(|frame| frame.into_data().ok())
                        .filter(|chunk| chunk.len() <= remaining)
                        .ok_or_else(BodyError::incomplete)
                });
                match chunk {
                    Ok(chunk) => {
                        let remaining = remaining - chunk.len();
                        let next = (remaining != 0).then_some((body, remaining, finished, failure));
                        Ok(Some((chunk, next)))
                    }
                    Err(error) => {
                        *failure.lock().expect("module upload failure") = Some(error);
                        Err(io::Error::other("module upload interrupted"))
                    }
                }
            },
        ))
    };
    let timeout_failure = failure.clone();
    let mut driver = JoinSet::new();
    driver.spawn(async move {
        tokio::select! {
            biased;
            _ = endpoint.disconnected.wait_for(|closed| *closed) => {}
            _ = connection => {}
            Err(_) = timeout_at(deadline, uploaded) => {
                *timeout_failure.lock().expect("module upload failure") = Some(BodyError::Timeout);
            }
        }
    });
    let response = sender
        .send_request(Request::from_parts(parts, upload))
        .await
        .map_err(|_| {
            failure
                .lock()
                .expect("module upload failure")
                .take()
                .map_or(BAD_GATEWAY, EntryError::from)
        })?;
    let (mut parts, body) = response.into_parts();
    if parts.status.is_informational()
        || parts.status.is_redirection() && parts.status != StatusCode::NOT_MODIFIED
    {
        return Err(BAD_GATEWAY);
    }
    strip_headers(&mut parts.headers, |name| {
        name.starts_with("access-control-")
            || matches!(name, "set-cookie" | "set-cookie2" | "location" | "refresh")
    });
    insert_security_headers(&mut parts.headers);
    let body = stream::unfold(
        (Body::new(body).into_data_stream(), driver),
        |(mut body, driver)| async move { body.next().await.map(|chunk| (chunk, (body, driver))) },
    );
    Ok(Response::from_parts(parts, Body::from_stream(body)))
}

fn strip_headers(headers: &mut HeaderMap, exclude: impl Fn(&str) -> bool) {
    let named: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .flat_map(|value| value.as_bytes().split(|&byte| byte == b','))
        .filter_map(|name| HeaderName::from_bytes(name.trim_ascii()).ok())
        .collect();
    let excluded: Vec<_> = headers
        .keys()
        .filter(|name| {
            named.contains(name)
                || matches!(
                    name.as_str(),
                    "connection"
                        | "keep-alive"
                        | "proxy-authenticate"
                        | "proxy-authorization"
                        | "proxy-connection"
                        | "te"
                        | "trailer"
                        | "transfer-encoding"
                        | "upgrade"
                )
                || exclude(name.as_str())
        })
        .cloned()
        .collect();
    for name in excluded {
        headers.remove(name);
    }
}
