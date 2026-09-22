//! Services providers.
use std::path::PathBuf;
use std::sync::Mutex;

use crate::app::ServerState;
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

/// One protocol/retry policy for HTTP SSE and downstream WebSocket turns.
pub(crate) fn open_external_stream(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    incoming: &std::collections::BTreeMap<String, String>,
    ids: &emp_router::ProjectionIds,
) -> Result<(emp_router::ExternalStream, ResolvedRoute), ExternalStreamOpenError> {
    let router = emp_router::ExternalRouter::new(&state.backend.transport.client);
    let candidates = emp_router::protocol_candidates(route);
    'candidate: for (index, protocol) in candidates.iter().copied().enumerate() {
        let candidate = route
            .with_protocol(protocol)
            .map_err(ExternalStreamOpenError::Route)?;
        for attempt in 0..2 {
            match state
                .backend
                .transport
                .runtime
                .block_on(router.open_stream(&candidate, body, incoming, ids))
            {
                Ok(stream) => return Ok((stream, candidate)),
                Err(error) => {
                    if error.error_class() == emp_transport::FailureClass::ContextLengthExceeded {
                        crate::services::context::record(state, &candidate, body, false);
                    }
                    if let Some(delay) =
                        crate::services::failures::external_retry_delay(&error, attempt, &candidate)
                    {
                        std::thread::sleep(delay);
                        continue;
                    }
                    if index + 1 < candidates.len()
                        && emp_transport::protocol_fallback_allowed(error.status(), false, false)
                    {
                        continue 'candidate;
                    }
                    let _usage = crate::services::usage::Observation::new(
                        state,
                        &candidate,
                        body,
                        incoming,
                        None,
                        "responses",
                    );
                    return Err(ExternalStreamOpenError::Router(error));
                }
            }
        }
    }
    Err(ExternalStreamOpenError::Unsupported)
}
