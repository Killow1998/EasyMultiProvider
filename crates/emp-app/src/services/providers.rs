//! Services providers.
use crate::app::ServerState;
use emp_core::ResolvedRoute;
use emp_state::VaultStore;
use emp_state::provider_api_key;
use emp_state::remember_resolved_protocol;
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
    let Ok(mut config) = state.backend.configuration.edit() else {
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
    let _ = config.commit(&updated);
}
