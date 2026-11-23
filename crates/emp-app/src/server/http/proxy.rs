use super::{error::EntryError, management::same_origin, raw_head};
use crate::{
    app::ServerState,
    http::{
        auth::{constant_time_eq, valid_caller_authorization},
        request::{Request, RequestMethod, RequestSource},
    },
    server::run_blocking,
    services::{gateway_clients::Caller, updates::RequestPermit},
    util::system_now,
};
use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::request::Parts,
};
use std::{
    collections::BTreeMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

fn proxy_allowed(request: Request<'_>, state: &ServerState, now: f64) -> bool {
    request.source.map_or_else(
        || proxy_identity(request, state, now, None).is_some(),
        |source| source.caller.is_some(),
    )
}

fn aero_delegation(
    request: Request<'_>,
    state: &ServerState,
    peer: Option<std::net::IpAddr>,
) -> Option<crate::services::modules::aero::CallerDelegation> {
    if !peer.is_some_and(|address| address.is_loopback())
        || request.header("Origin").is_some()
        || !same_origin(request, state.port)
        || request.header_count("Authorization") != 1
        || !matches!(
            request.raw_path(),
            "/v1/responses"
                | "/v1/responses/compact"
                | "/v1/images/generations"
                | "/v1/images/edits"
                | "/v1/audio/transcriptions"
                | "/v1/realtime/calls"
        )
    {
        return None;
    }
    let key = request.header("Authorization")?.strip_prefix("Bearer ")?;
    state.backend.aero_callers.identify(key)
}

pub(crate) fn proxy_identity(
    request: Request<'_>,
    state: &ServerState,
    now: f64,
    peer: Option<std::net::IpAddr>,
) -> Option<crate::services::gateway_clients::Caller> {
    if request.header_count("Authorization") > 1 {
        return None;
    }
    let token = request
        .header("Authorization")
        .and_then(|value| value.strip_prefix("Bearer "));
    let gateway = token
        .filter(|_| state.backend.modules.active(emp_modules::Module::Gateway))
        .and_then(|value| {
            if constant_time_eq(value.as_bytes(), state.backend.gateway_key.as_bytes()) {
                Some(Caller {
                    id: "shared".into(),
                    name: String::new(),
                })
            } else {
                state
                    .backend
                    .gateway_clients
                    .identify(value, &state.backend.modules)
            }
        });
    if gateway.is_some() && request.header("Origin").is_none() {
        return gateway;
    }
    if !same_origin(request, state.port) {
        return None;
    }
    let supplied_session = request.session_token();
    if gateway.is_some() {
        return gateway;
    }
    let local_decision = matches!(request.raw_path(), "/v1/decisions" | "/v1/systemone")
        && state.backend.modules.active(emp_modules::Module::Decision)
        && token.is_some_and(|value| {
            constant_time_eq(value.as_bytes(), state.backend.gateway_key.as_bytes())
        });
    let local_image = request.method == RequestMethod::Post
        && matches!(
            request.raw_path(),
            "/v1/images/generations" | "/v1/images/edits"
        )
        && peer.is_some_and(|peer| peer.is_loopback())
        && request.header("Origin").is_none()
        && token.is_some_and(|value| {
            constant_time_eq(value.as_bytes(), state.backend.gateway_key.as_bytes())
        });
    (local_decision
        || local_image
        || state.sessions.contains(supplied_session.as_deref(), now)
        || valid_caller_authorization(
            request.header("Authorization"),
            &state.backend.accounts.native_auth_path,
        ))
    .then(|| Caller {
        id: "local".into(),
        name: String::new(),
    })
}

pub(crate) fn request_source(
    request: Request<'_>,
    state: &ServerState,
    now: f64,
    peer: Option<IpAddr>,
) -> RequestSource {
    let delegation = aero_delegation(request, state, peer);
    let caller = delegation
        .as_ref()
        .map(|delegate| delegate.caller.clone())
        .or_else(|| proxy_identity(request, state, now, peer));
    RequestSource {
        caller,
        peer,
        delegation,
    }
}

pub(crate) fn require_model_catalog(
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) -> Result<(), EntryError> {
    if same_origin(request, state.port) || proxy_allowed(request, state, now) {
        return Ok(());
    }
    Err(EntryError::CrossOrigin(
        "cross-origin model catalog request rejected",
    ))
}

pub(crate) fn require_proxy(
    request: Request<'_>,
    state: &ServerState,
    now: f64,
) -> Result<(), EntryError> {
    if proxy_allowed(request, state, now) {
        return Ok(());
    }
    let status = if same_origin(request, state.port) {
        401
    } else {
        403
    };
    match request.raw_path() {
        "/v1/decisions" | "/v1/systemone" => Err(EntryError::Json(
            401,
            "EMP gateway authentication is required",
            None,
        )),
        "/v1/live" if status == 401 && !crate::api::realtime::has_native_subscription(state) => {
            Err(crate::api::realtime::native_subscription_unavailable().into())
        }
        path if path == "/v1/live" || path.starts_with("/v1/live/") => {
            Err(crate::api::realtime::RealtimeError::new(
                status,
                "realtime_caller_unauthorized",
                "Caller authentication is required for Codex Voice",
            )
            .into())
        }
        _ => Err(EntryError::Json(
            status,
            "proxy caller authentication is required",
            None,
        )),
    }
}

pub(crate) struct RequestAccess {
    _permit: Option<RequestPermit>,
}

impl RequestAccess {
    pub(crate) fn require(request: Request<'_>, state: &ServerState) -> Result<Self, EntryError> {
        let path = request.raw_path();
        if let Some(error) = crate::api::modules::unavailable(path, state) {
            return Err(error);
        }
        if (request.method == RequestMethod::Post && !path.starts_with("/api/updates/"))
            || request.method == RequestMethod::Delete
        {
            return state
                .updates
                .enter()
                .map(|permit| Self {
                    _permit: Some(permit),
                })
                .ok_or(EntryError::Updating);
        }
        Ok(Self { _permit: None })
    }
}

impl FromRequestParts<Arc<ServerState>> for RequestAccess {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        Self::require(raw_head(&parts.extensions)?.request(), state)
    }
}

fn peer(parts: &Parts) -> Option<IpAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip())
}

pub(crate) struct ProxyCaller {
    pub(crate) caller: Caller,
    pub(crate) incoming: BTreeMap<String, String>,
}

impl FromRequestParts<Arc<ServerState>> for ProxyCaller {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(&parts.extensions)?.clone();
        let peer = peer(parts);
        let state = state.clone();
        run_blocking(move || {
            let mut request = raw.request();
            let now = system_now();
            let source = request_source(request, &state, now, peer);
            request.source = Some(&source);
            require_proxy(request, &state, now)?;
            let incoming = request.incoming();
            let caller = source.caller.expect("authorized proxy caller");
            Ok(Self { caller, incoming })
        })
        .await
        .map_err(|_| EntryError::internal())?
    }
}

pub(crate) struct ModelCatalog;

impl FromRequestParts<Arc<ServerState>> for ModelCatalog {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(&parts.extensions)?.clone();
        let peer = peer(parts);
        let state = state.clone();
        run_blocking(move || {
            let mut request = raw.request();
            let now = system_now();
            let source = request_source(request, &state, now, peer);
            request.source = Some(&source);
            require_model_catalog(request, &state, now)?;
            Ok(Self)
        })
        .await
        .map_err(|_| EntryError::internal())?
    }
}
