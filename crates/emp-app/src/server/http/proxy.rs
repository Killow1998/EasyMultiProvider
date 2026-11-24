use super::{error::EntryError, management::same_origin, raw_head};
use crate::{
    app::ServerState,
    http::{
        auth::{constant_time_eq, valid_caller_authorization},
        request::{Request, RequestMethod, RequestSource},
    },
    server::run_blocking,
    services::{
        connection_admission::BusinessToken, gateway_clients::Caller,
        modules::aero::CallerDelegation, updates::RequestPermit,
    },
    util::system_now,
};
use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::request::Parts,
};
use emp_modules::Module;
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

fn aero_delegation_key<'a>(
    request: Request<'a>,
    state: &ServerState,
    peer: Option<std::net::IpAddr>,
) -> Option<&'a str> {
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
    request.header("Authorization")?.strip_prefix("Bearer ")
}

#[derive(Clone)]
pub(crate) struct AdmittedCallback {
    pub(crate) identity: CallerDelegation,
    pub(crate) work: Option<Arc<RequestPermit>>,
}

pub(crate) enum ConnectionIntake {
    Public(BusinessToken),
    CallbackHead(tokio::sync::OwnedSemaphorePermit),
}

impl ConnectionIntake {
    pub(crate) fn accept(state: &ServerState, peer: IpAddr) -> Option<Self> {
        let callers = &state.backend.aero_callers;
        if peer.is_loopback()
            && callers.classifies_callbacks()
            && let Some(header) = callers.acquire_header()
        {
            return Some(Self::CallbackHead(header));
        }
        state
            .lifecycle
            .admission
            .acquire_request()
            .map(Self::Public)
    }

    pub(crate) fn admit(
        self,
        request: Option<Request<'_>>,
        state: &ServerState,
        peer: Option<IpAddr>,
    ) -> Option<(BusinessToken, Option<AdmittedCallback>)> {
        let callback = request
            .filter(|request| request.method == RequestMethod::Post)
            .and_then(|request| aero_delegation_key(request, state, peer))
            .and_then(|key| {
                state
                    .backend
                    .aero_callers
                    .identify_callback(key, &state.lifecycle.test_hooks)
            });
        if let Some((identity, Some(lease))) = callback {
            drop(self);
            return Some((
                lease.business,
                Some(AdmittedCallback {
                    identity,
                    work: Some(lease.work),
                }),
            ));
        }

        match self {
            Self::Public(permit) => Some((permit, None)),
            Self::CallbackHead(header) => {
                drop(header);
                let permit = state.lifecycle.admission.acquire_request()?;
                let identity = callback.map(|(identity, _)| AdmittedCallback {
                    identity,
                    work: None,
                });
                Some((permit, identity))
            }
        }
    }
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
    admitted: Option<CallerDelegation>,
) -> RequestSource {
    let delegation = admitted.or_else(|| {
        let key = aero_delegation_key(request, state, peer)?;
        state.backend.aero_callers.identify(key)
    });
    let caller = delegation
        .as_ref()
        .map(|delegate| delegate.caller.clone())
        .or_else(|| proxy_identity(request, state, now, peer));
    RequestSource {
        caller,
        peer,
        delegation,
        aero_lease: None,
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
    pub(crate) permit: Option<Arc<RequestPermit>>,
}

fn required_module(path: &str) -> Option<Module> {
    match path {
        "/v1/decisions"
        | "/v1/systemone"
        | "/api/decisions/models"
        | "/api/decisions/connection" => Some(Module::Decision),
        "/api/gateway" | "/api/gateway/clients" | "/api/gateway/usage" => Some(Module::Gateway),
        _ if path.starts_with("/api/team/") => Some(Module::Team),
        _ => None,
    }
}

impl RequestAccess {
    pub(crate) fn require(
        request: Request<'_>,
        state: &ServerState,
        work: Option<&Arc<RequestPermit>>,
    ) -> Result<Self, EntryError> {
        Self::require_with_module(request, state, required_module(request.raw_path()), work)
    }

    fn require_with_module(
        request: Request<'_>,
        state: &ServerState,
        module: Option<Module>,
        work: Option<&Arc<RequestPermit>>,
    ) -> Result<Self, EntryError> {
        let path = request.raw_path();
        if let Some(module) = module
            && !state.backend.modules.active(module)
        {
            return Err(EntryError::Json(
                404,
                "This EMP module is not active",
                Some("module_disabled"),
            ));
        }
        if (request.method == RequestMethod::Post && !path.starts_with("/api/updates/"))
            || request.method == RequestMethod::Delete
        {
            if let Some(work) = work {
                return Ok(Self {
                    permit: Some(work.clone()),
                });
            }
            return state
                .lifecycle
                .updates()
                .enter()
                .map(|permit| Self {
                    permit: Some(Arc::new(permit)),
                })
                .ok_or(EntryError::Updating);
        }
        Ok(Self { permit: None })
    }
}

impl FromRequestParts<Arc<ServerState>> for RequestAccess {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let raw = raw_head(&parts.extensions)?;
        let request = raw.request();
        let work = parts
            .extensions
            .get::<AdmittedCallback>()
            .and_then(|call| call.work.as_ref());
        let Some(module) = required_module(request.raw_path()) else {
            return Self::require_with_module(request, state, None, work);
        };
        let raw = raw.clone();
        let state = state.clone();
        run_blocking(move || Self::require_with_module(raw.request(), &state, Some(module), None))
            .await
            .map_err(|_| EntryError::internal())?
    }
}

fn peer(parts: &Parts) -> Option<IpAddr> {
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip())
}

pub(crate) struct ProxyCaller {
    #[cfg(test)]
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
        let delegation = parts
            .extensions
            .get::<AdmittedCallback>()
            .map(|call| call.identity.clone());
        let state = state.clone();
        run_blocking(move || {
            let mut request = raw.request();
            let now = system_now();
            let source = request_source(request, &state, now, peer, delegation);
            request.source = Some(&source);
            require_proxy(request, &state, now)?;
            let incoming = request.incoming();
            Ok(Self {
                #[cfg(test)]
                caller: source.caller.expect("authorized proxy caller"),
                incoming,
            })
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
            let source = request_source(request, &state, now, peer, None);
            request.source = Some(&source);
            require_model_catalog(request, &state, now)?;
            Ok(Self)
        })
        .await
        .map_err(|_| EntryError::internal())?
    }
}
