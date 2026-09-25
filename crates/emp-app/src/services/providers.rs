//! Services providers.
use std::path::PathBuf;
use std::sync::Mutex;

use crate::app::ServerState;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::disconnect::DisconnectRace;
use emp_core::ResolvedRoute;
use emp_state::VaultStore;
use emp_state::load_configuration;
use emp_state::provider_api_key;
use emp_state::remember_resolved_protocol;
use emp_state::save_configuration;
use serde_json::Value;

pub(crate) fn hydrate_provider_keys(config: &mut Value, vault: &VaultStore) {
    let Some(providers) = config.get_mut("providers").and_then(Value::as_array_mut) else {
        return;
    };
    for provider in providers {
        let key = provider_api_key(provider, vault);
        if let Some(provider) = provider.as_object_mut() {
            provider.insert("api_key".to_owned(), Value::String(key));
        }
    }
}

pub(crate) fn persist_protocol_observation(state: &ServerState, route: &ResolvedRoute) {
    let Ok(mut config) = state.backend.configuration.config.lock() else {
        return;
    };
    let Ok(Some(updated)) = remember_resolved_protocol(
        &config,
        &route.provider_id,
        &route.requested_model,
        route.protocol.as_config_str(),
    ) else {
        return;
    };
    if save_configuration(
        &updated,
        Some(&state.backend.configuration.config_path),
        &state.backend.configuration.vault,
    )
    .is_err()
    {
        return;
    }
    if let Ok(reloaded) = load_configuration(Some(&state.backend.configuration.config_path)) {
        *config = reloaded;
    }
}

pub(crate) struct ConfigurationState {
    pub(crate) config: Mutex<Value>,
    pub(crate) discovery_lock: Mutex<()>,
    pub(crate) config_path: PathBuf,
    pub(crate) vault: VaultStore,
}

pub(crate) enum ExternalStreamOpenError {
    Router(emp_router::RouterError),
    Route(emp_core::RouteResolutionError),
    Unsupported,
}

pub(crate) enum CancellableExternalStreamOpen {
    Opened(Box<emp_router::ExternalStream>, ResolvedRoute),
    Disconnected,
}

/// One protocol/retry policy for HTTP SSE and downstream WebSocket turns.
pub(crate) fn open_external_stream(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &std::collections::BTreeMap<String, String>,
    ids: &emp_router::ProjectionIds,
) -> Result<(emp_router::ExternalStream, ResolvedRoute), ExternalStreamOpenError> {
    match open_external_stream_with_monitor(state, route, body, incoming, ids, None)? {
        CancellableExternalStreamOpen::Opened(stream, candidate) => Ok((*stream, candidate)),
        CancellableExternalStreamOpen::Disconnected => {
            unreachable!("non-cancellable open cannot observe a disconnect")
        }
    }
}

pub(crate) fn open_external_stream_cancellable(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &std::collections::BTreeMap<String, String>,
    ids: &emp_router::ProjectionIds,
    monitor: &mut DisconnectMonitor,
) -> Result<CancellableExternalStreamOpen, ExternalStreamOpenError> {
    open_external_stream_with_monitor(state, route, body, incoming, ids, Some(monitor))
}

fn open_external_stream_with_monitor(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &std::collections::BTreeMap<String, String>,
    ids: &emp_router::ProjectionIds,
    mut monitor: Option<&mut DisconnectMonitor>,
) -> Result<CancellableExternalStreamOpen, ExternalStreamOpenError> {
    let router = emp_router::ExternalRouter::new(&state.backend.transport.client);
    let candidates = emp_router::protocol_candidates(route);
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(ExternalStreamOpenError::Route)?;
        for attempt in 0..3 {
            let opened =
                match monitor.as_deref_mut() {
                    Some(monitor) => state.backend.transport.runtime.block_on(
                        monitor.race(router.open_stream(&candidate, body, incoming, ids)),
                    ),
                    None => DisconnectRace::Ready(
                        state
                            .backend
                            .transport
                            .runtime
                            .block_on(router.open_stream(&candidate, body, incoming, ids)),
                    ),
                };
            let opened = match opened {
                DisconnectRace::Ready(result) => result,
                DisconnectRace::Disconnected => {
                    return Ok(CancellableExternalStreamOpen::Disconnected);
                }
            };
            match opened {
                Ok(stream) => {
                    return Ok(CancellableExternalStreamOpen::Opened(
                        Box::new(stream),
                        candidate,
                    ));
                }
                Err(error) => {
                    if error.error_class() == emp_transport::FailureClass::ContextLengthExceeded {
                        crate::services::context::record(state, &candidate, body, false);
                    }
                    if let Some(delay) =
                        crate::services::failures::external_retry_delay(&error, attempt, &candidate)
                    {
                        let delay_elapsed = match monitor.as_deref_mut() {
                            Some(monitor) => matches!(
                                state.backend.transport.runtime.block_on(
                                    monitor.race(async { tokio::time::sleep(delay).await })
                                ),
                                DisconnectRace::Ready(()),
                            ),
                            None => {
                                std::thread::sleep(delay);
                                true
                            }
                        };
                        if !delay_elapsed {
                            return Ok(CancellableExternalStreamOpen::Disconnected);
                        }
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && emp_transport::protocol_fallback_allowed(error.status(), false, false)
                    {
                        continue 'candidate;
                    }
                    let mut usage = crate::services::observation::Observation::new(
                        state,
                        &candidate,
                        body,
                        incoming,
                        None,
                        "responses",
                    );
                    usage.router_error(&error);
                    return Err(ExternalStreamOpenError::Router(error));
                }
            }
        }
    }
    Err(ExternalStreamOpenError::Unsupported)
}
