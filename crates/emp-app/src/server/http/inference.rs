use super::{
    body::JsonBody,
    error::{EntryError, HttpReply, no_response},
    proxy::{AdmittedCallback, RequestAccess, request_source, require_proxy},
    raw_head,
    stream::{BodyOutput, StreamConnection},
};
use crate::{
    api::{compact, decisions, images, responses, search, streaming::ReplySink},
    app::ServerState,
    http::{
        auth::same_origin,
        request::{Request, RequestMethod, read_json_body},
    },
    server::{connection::ConnectionContext, run_blocking},
    services::observation::request::{Phase, RequestObservation},
    util::system_now,
};
use axum::{
    Extension, Router,
    body::Bytes,
    extract::{ConnectInfo, FromRequest, Request as AxumRequest, State},
    response::Response,
    routing::post,
};
use serde_json::Value;
use std::{
    net::{SocketAddr, TcpStream},
    sync::{Arc, Mutex, OnceLock},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy)]
pub(crate) enum InferenceRoute {
    Responses,
    Compact,
    Decisions,
    Images(&'static str),
    Search,
}

pub(crate) enum InferenceOutcome {
    Buffered(HttpReply),
    Closed,
    Streamed,
}

impl From<HttpReply> for InferenceOutcome {
    fn from(reply: HttpReply) -> Self {
        Self::Buffered(reply)
    }
}

impl From<Option<HttpReply>> for InferenceOutcome {
    fn from(reply: Option<HttpReply>) -> Self {
        reply.map_or(Self::Closed, Self::Buffered)
    }
}

const ROUTES: &[(&str, InferenceRoute)] = &[
    ("/v1/responses", InferenceRoute::Responses),
    ("/v1/responses/compact", InferenceRoute::Compact),
    ("/v1/decisions", InferenceRoute::Decisions),
    ("/v1/systemone", InferenceRoute::Decisions),
    (
        "/v1/images/generations",
        InferenceRoute::Images("generations"),
    ),
    ("/v1/images/edits", InferenceRoute::Images("edits")),
    ("/v1/alpha/search", InferenceRoute::Search),
];

impl InferenceRoute {
    pub(crate) fn for_request(request: Request<'_>) -> Option<Self> {
        if request.method != RequestMethod::Post {
            return None;
        }
        ROUTES
            .iter()
            .find_map(|&(path, route)| (path == request.raw_path()).then_some(route))
    }

    fn authorize(
        self,
        request: Request<'_>,
        state: &ServerState,
        now: f64,
        observation: Option<&mut RequestObservation>,
    ) -> Result<(), HttpReply> {
        require_proxy(request, state, now).map_err(EntryError::reply)?;
        if matches!(self, Self::Decisions) {
            decisions::validate_headers(request)?;
        }
        if let Some(observation) = observation {
            if matches!(self, Self::Responses)
                && same_origin(request, state.port)
                && state
                    .sessions
                    .contains(request.session_token().as_deref(), now)
                && let Some(kind) = request.header("x-emp-probe")
                && emp_domain::MODEL_TEST_OPERATIONS.contains(&kind)
            {
                observation.probe(kind);
            }
            observation.phase(Phase::ReadBody);
        }
        Ok(())
    }

    fn execute(
        self,
        sink: &mut ReplySink<'_>,
        request: Request<'_>,
        body: Value,
        state: &ServerState,
        observation: Option<&mut RequestObservation>,
    ) -> InferenceOutcome {
        match self {
            Self::Responses => responses::execute(
                sink,
                request,
                body,
                state,
                observation.expect("model observation"),
            ),
            Self::Compact => compact::execute(
                sink,
                request,
                body,
                state,
                observation.expect("model observation"),
            ),
            Self::Decisions => decisions::execute(sink.socket(), request, body, state).into(),
            Self::Images(operation) => {
                images::execute(sink.socket(), request, body, state, operation).into()
            }
            Self::Search => search::execute(request, body, state).into(),
        }
    }

    pub(crate) fn legacy(
        self,
        stream: &mut TcpStream,
        request: Request<'_>,
        prefix: Vec<u8>,
        state: &ServerState,
        now: f64,
        mut observation: Option<&mut RequestObservation>,
    ) -> Option<Vec<u8>> {
        if let Err(reply) = self.authorize(request, state, now, observation.as_deref_mut()) {
            return Some(reply.wire_response());
        }
        let body = match read_json_body(stream, request, prefix, state) {
            Ok(body) => body,
            Err(error) => return Some(EntryError::from(error).wire_response()),
        };
        match self.execute(
            &mut ReplySink::Legacy(stream),
            request,
            body,
            state,
            observation,
        ) {
            InferenceOutcome::Buffered(reply) => Some(reply.wire_response()),
            InferenceOutcome::Closed => Some(Vec::new()),
            InferenceOutcome::Streamed => None,
        }
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    ROUTES.iter().fold(Router::new(), |router, &(path, route)| {
        router.route(path, post(handle).layer(Extension(route)))
    })
}

pub(in crate::server) struct InferenceConnection {
    pub(in crate::server) socket: Arc<TcpStream>,
    pub(in crate::server) stream: Arc<StreamConnection>,
    pub(in crate::server) model: Mutex<Option<RequestObservation>>,
    pub(in crate::server) receipt: OnceLock<BufferedReceipt>,
}

pub(in crate::server) enum BufferedReceipt {
    Response { status: u16, body: Bytes },
    Closed,
}

async fn handle(
    State(state): State<Arc<ServerState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(route): Extension<InferenceRoute>,
    Extension(context): Extension<ConnectionContext>,
    Extension(inference): Extension<Arc<InferenceConnection>>,
    access: Result<RequestAccess, EntryError>,
    request: AxumRequest,
) -> Response {
    match submit(&state, peer, route, context, &inference, access, request).await {
        Ok(response) => response,
        Err(reply) => buffered(
            reply,
            inference.model.lock().expect("model observation").as_ref(),
            &inference.receipt,
        ),
    }
}

async fn submit(
    state: &Arc<ServerState>,
    peer: SocketAddr,
    route: InferenceRoute,
    context: ConnectionContext,
    inference: &Arc<InferenceConnection>,
    access: Result<RequestAccess, EntryError>,
    request: AxumRequest,
) -> Result<Response, HttpReply> {
    context.retain_request_access(access.map_err(EntryError::reply)?);
    let raw = raw_head(request.extensions())
        .map_err(EntryError::reply)?
        .clone();
    let delegation = request
        .extensions()
        .get::<AdmittedCallback>()
        .map(|call| call.identity.clone());
    let lease = context.aero_call_lease();
    let source = run_blocking({
        let state = state.clone();
        let inference = inference.clone();
        let raw = raw.clone();
        move || {
            let now = system_now();
            let mut request = raw.request();
            let mut source = request_source(request, &state, now, Some(peer.ip()), delegation);
            source.aero_lease = lease;
            request.source = Some(&source);
            route.authorize(
                request,
                &state,
                now,
                inference.model.lock().expect("model observation").as_mut(),
            )?;
            Ok::<_, HttpReply>(source)
        }
    })
    .await
    .map_err(|_| EntryError::internal().reply())??;

    let JsonBody(body) = JsonBody::<Value>::from_request(request, state)
        .await
        .map_err(EntryError::reply)?;
    let (sender, response) = oneshot::channel();
    let delivery = inference.stream.delivery.clone();
    let worker = tokio::task::spawn_blocking({
        let state = state.clone();
        let inference = inference.clone();
        move || {
            let mut observation = inference.model.lock().expect("model observation").take();
            let mut request = raw.request();
            request.source = Some(&source);
            request.observation_id = context.request_id();
            let mut sink = BodyOutput::new(inference.socket.clone(), delivery, sender);
            let reply = route.execute(
                &mut ReplySink::Candidate(&mut sink),
                request,
                body,
                &state,
                observation.as_mut(),
            );
            sink.finish(|| match reply {
                InferenceOutcome::Buffered(reply) => {
                    buffered(reply, observation.as_ref(), &inference.receipt)
                }
                InferenceOutcome::Closed => {
                    let _ = inference.receipt.set(BufferedReceipt::Closed);
                    no_response()
                }
                InferenceOutcome::Streamed => no_response(),
            });
            *inference.model.lock().expect("model observation") = observation;
        }
    });
    *inference.stream.worker.lock().expect("stream worker") = Some(worker);
    response.await.map_err(|_| EntryError::internal().reply())
}

fn buffered(
    mut reply: HttpReply,
    model: Option<&RequestObservation>,
    receipt: &OnceLock<BufferedReceipt>,
) -> Response {
    if let Some(model) = model {
        reply.present(model.errors());
    }
    let _ = receipt.set(reply.receipt());
    reply.into_http_response()
}
