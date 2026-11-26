use super::{
    body::{BodyError, MANAGEMENT_BODY_LIMIT, body_size_error, read_frame, request_length},
    error::{EntryError, JsonReply, insert_security_headers},
    management::ManagementSession,
    proxy::{RequestAccess, local_caller, request_source, require_local_proxy, require_proxy},
};
use crate::{
    app::ServerState,
    http::request::{module_runtime_target, request_body_timeout},
    modules::{process::DataEndpoint, registry::HttpRoute},
    server::{
        connection::{ConnectionContext, ForwardCancellation},
        head::RawRequestHead,
        run_blocking,
    },
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
use emp_transport::TimeoutPolicy;
use futures_util::{StreamExt as _, stream};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use std::{
    convert::Infallible,
    future::pending,
    io,
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::{
    net::TcpStream,
    sync::{oneshot, watch},
    task::JoinSet,
    time::{Instant, timeout, timeout_at},
};

const UNAVAILABLE: EntryError = EntryError::Json(503, "module data endpoint is unavailable", None);
const BAD_GATEWAY: EntryError = EntryError::Json(502, "module data request failed", None);
const CORE_REQUEST_PREFIXES: &[&str] = &["x-emp-", "x-aero-", "x-forwarded-", "sec-fetch-"];
const CORE_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "content-length",
    "cookie",
    "cookie2",
    "forwarded",
    "host",
    "origin",
    "referer",
    "x-real-ip",
];
const HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

impl From<ForwardCancellation> for EntryError {
    fn from(cause: ForwardCancellation) -> Self {
        match cause {
            ForwardCancellation::Generation => UNAVAILABLE,
            ForwardCancellation::UploadTimeout => BodyError::Timeout.into(),
            ForwardCancellation::Peer | ForwardCancellation::ResponseTimeout => BAD_GATEWAY,
        }
    }
}

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
    forward(incoming, target, raw, endpoint, route, &connection, length).await
}

async fn forward(
    request: Request,
    target: Uri,
    raw: RawRequestHead,
    mut endpoint: DataEndpoint,
    route: &HttpRoute,
    public: &ConnectionContext,
    length: usize,
) -> Result<Response, EntryError> {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port.get()));
    let deadline = Instant::now() + request_body_timeout(length);
    let forwarding = public.module_forwarding();
    let (mut parts, body) = request.into_parts();
    let has_content_length = parts.headers.contains_key(header::CONTENT_LENGTH);
    parts.uri = target;
    parts.version = Version::HTTP_11;
    parts.extensions.clear();
    strip_headers(&mut parts.headers, |name| {
        CORE_REQUEST_HEADERS.contains(&name)
            || CORE_REQUEST_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
    });
    parts.headers.insert(
        header::HOST,
        HeaderValue::from_str(&address.to_string()).expect("loopback host"),
    );
    parts
        .headers
        .insert(header::AUTHORIZATION, endpoint.authorization);
    parts.headers.insert(ROUTE_ID_HEADER, route.id.clone());
    if let Some(request_id) = public.request_id() {
        parts.headers.insert(
            "x-emp-request-id",
            HeaderValue::from_str(request_id).expect("hex request ID"),
        );
    }
    parts
        .headers
        .insert(header::CONNECTION, HeaderValue::from_static("close"));
    if has_content_length {
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }

    let failure = Arc::new(Mutex::new(None));
    let (upload_finished, uploaded) = oneshot::channel::<()>();
    let upload = if length == 0 {
        drop(upload_finished);
        Body::empty()
    } else {
        Body::from_stream(stream::try_unfold(
            Some((
                body,
                length,
                upload_finished,
                failure.clone(),
                forwarding.signal.clone(),
            )),
            move |state| async move {
                let Some((mut body, remaining, finished, failure, signal)) = state else {
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
                        let next = (remaining != 0)
                            .then_some((body, remaining, finished, failure, signal));
                        Ok(Some((chunk, next)))
                    }
                    Err(error) => {
                        if matches!(error, BodyError::Timeout) {
                            signal.cancel(ForwardCancellation::UploadTimeout);
                        }
                        *failure.lock().expect("module upload failure") = Some(error);
                        Err(io::Error::other("module upload interrupted"))
                    }
                }
            },
        ))
    };
    let timeouts = TimeoutPolicy::default();
    let (progress, mut progressed) = watch::channel(None::<Instant>);
    let watch_progress = progress.clone();
    let deadlines = async move {
        let _progress = watch_progress;
        if timeout_at(deadline, uploaded).await.is_err() {
            return ForwardCancellation::UploadTimeout;
        }
        if timeout(timeouts.first_event, progressed.wait_for(Option::is_some))
            .await
            .is_ok()
        {
            loop {
                let deadline = progressed
                    .borrow_and_update()
                    .expect("module response deadline");
                if timeout_at(deadline, progressed.changed()).await.is_err() {
                    break;
                }
            }
        }
        ForwardCancellation::ResponseTimeout
    };
    let signal = forwarding.signal.clone();
    let (ready, sender) = oneshot::channel::<http1::SendRequest<Body>>();
    let exchange = async move {
        let socket = TcpStream::connect(address).await?;
        let (sender, connection) = http1::handshake(TokioIo::new(socket))
            .await
            .map_err(io::Error::other)?;
        if ready.send(sender).is_ok() {
            let _ = connection.await;
        }
        Ok::<_, io::Error>(())
    };
    let mut driver = JoinSet::new();
    driver.spawn(async move {
        let cause = tokio::select! {
            biased;
            _ = signal.cancelled() => return,
            _ = endpoint.disconnected.wait_for(|closed| *closed) => ForwardCancellation::Generation,
            cause = deadlines => cause,
            never = async {
                let _ = exchange.await;
                pending::<Infallible>().await
            } => match never {},
        };
        signal.cancel(cause);
    });
    let response = tokio::select! {
        biased;
        cause = forwarding.signal.cancelled() => return Err(cause.into()),
        response = async {
            let mut sender = sender.await.map_err(|_| BAD_GATEWAY)?;
            raw.allow_body();
            sender
                .send_request(Request::from_parts(parts, upload))
                .await
                .map_err(|_| {
                    failure
                        .lock()
                        .expect("module upload failure")
                        .take()
                        .map_or(BAD_GATEWAY, EntryError::from)
                })
        } => response?,
    };
    let (mut parts, body) = response.into_parts();
    if parts.status.is_informational()
        || parts.status.is_redirection() && parts.status != StatusCode::NOT_MODIFIED
    {
        return Err(BAD_GATEWAY);
    }
    let length = response_length(&parts.headers)?;
    strip_headers(&mut parts.headers, |name| {
        name.starts_with("access-control-")
            || matches!(
                name,
                "content-length" | "set-cookie" | "set-cookie2" | "location" | "refresh"
            )
    });
    if let Some(length) = length {
        parts
            .headers
            .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
    insert_security_headers(&mut parts.headers);
    progress.send_replace(Some(Instant::now() + timeouts.stream_idle));
    let body = Body::new(body).into_data_stream().map(move |chunk| {
        progress.send_replace(Some(Instant::now() + timeouts.stream_idle));
        chunk
    });
    assert!(forwarding.driver.set(driver).is_ok());
    Ok(Response::from_parts(parts, Body::from_stream(body)))
}

fn response_length(headers: &HeaderMap) -> Result<Option<u64>, EntryError> {
    let encodings = headers.get_all(header::TRANSFER_ENCODING);
    let lengths = headers.get_all(header::CONTENT_LENGTH);
    let mut encodings = encodings.iter();
    if let Some(encoding) = encodings.next() {
        return if encoding.as_bytes().eq_ignore_ascii_case(b"chunked")
            && encodings.next().is_none()
            && lengths.iter().next().is_none()
        {
            Ok(None)
        } else {
            Err(BAD_GATEWAY)
        };
    }

    let mut length = None;
    for value in lengths.iter() {
        let value = value.to_str().map_err(|_| BAD_GATEWAY)?;
        for value in value.split(',').map(str::trim) {
            let parsed = value.parse::<u64>().map_err(|_| BAD_GATEWAY)?;
            if !value.bytes().all(|byte| byte.is_ascii_digit())
                || length.is_some_and(|previous| previous != parsed)
            {
                return Err(BAD_GATEWAY);
            }
            length = Some(parsed);
        }
    }
    length.map(Some).ok_or(BAD_GATEWAY)
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
            named.contains(name) || HOP_HEADERS.contains(&name.as_str()) || exclude(name.as_str())
        })
        .cloned()
        .collect();
    for name in excluded {
        headers.remove(name);
    }
}
